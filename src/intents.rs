use crate::client::Client;
use crate::context;
use crate::discovery::{get_tool as find_tool, is_stale_tool_result, ToolDiscovery};
use crate::errors::{ErrorType, HaCliError};
use crate::models::Entity;
use crate::resolver::{self, Resolution};
use crate::security;
use crate::tool_result::{extract_text, success_failure_message, tool_error_message, truthy};
use serde_json::{json, Map, Value as Json};

/// Read-only инструмент ha-mcp для чтения состояния по внутренне
/// разрешённому ID (docs/mcp_migration.md, этап 3, подпункт 3).
pub const HA_GET_STATE_TOOL: &str = "ha_get_state";

pub const INITIAL_INTENT_SET: &[&str] = &[
    "HassTurnOn",
    "HassTurnOff",
    "HassGetState",
    "HassLightSet",
    "HassSetPosition",
];

pub fn validate_intent(intent: &str) -> Result<(), HaCliError> {
    if INITIAL_INTENT_SET.contains(&intent) {
        return Ok(());
    }
    Err(HaCliError::new(
        ErrorType::InvalidArguments,
        format!(
            "unknown or blocked intent: {intent}; allowed: {}",
            INITIAL_INTENT_SET.join(", ")
        ),
    ))
}

/// Перенос `execute`.
pub fn execute(client: &mut Client, intent: &str, payload: &Json) -> Result<Json, HaCliError> {
    validate_intent(intent)?;
    if let Err(message) = security::validate_entity_payload(payload) {
        return Err(HaCliError::new(ErrorType::InvalidArguments, message));
    }
    // Этап 3.3 (docs/mcp_migration.md): на ha-mcp (`mcp_url` задан)
    // HassGetState разрешается строго по отфильтрованному каталогу
    // `ha_search`, состояние читается `ha_get_state` по внутренним ID.
    // Прочие действия после разрешения возвращают «not implemented yet» —
    // путь исполнения (ha_call_service) появится на этапе 4.
    // Assist endpoint без `mcp_url` работает как раньше.
    if client.config.mcp_url.is_some() {
        // Этап 4.1: строгая интент-специфичная валидация payload
        // (allowlist ключей, типы, диапазоны, сочетания, домен по
        // селектору) — ДО любых сетевых вызовов, включая
        // `resolver::prepare_action` и `ha_get_state`.
        if let Err(message) = security::validate_intent_payload(intent, payload) {
            return Err(HaCliError::new(ErrorType::InvalidArguments, message));
        }
        if intent == "HassGetState" {
            return execute_hamcp_get_state(client, payload);
        }
        resolver::prepare_action(client, payload)?;
        return Err(HaCliError::new(
            ErrorType::ToolNotFound,
            format!("ha-mcp execution for {intent} is not implemented yet"),
        ));
    }
    let mut discovery = ToolDiscovery::new(None);
    let tool = match discovery.get_tool(client, intent) {
        Ok(tool) => tool,
        Err(err) => {
            if !matches!(err.kind, ErrorType::ToolNotFound) {
                return Err(err);
            }
            if intent != "HassGetState" {
                return Err(err);
            }
            let live_context = context::get_live_context(client)?;
            return context::query_state(&live_context, payload);
        }
    };
    let arguments = normalize_arguments(payload, &tool.input_schema);
    let mut result = client.tools_call(&tool.mcp_name, &arguments)?;
    if is_stale_tool_result(&result) {
        let mapping = discovery.refresh(client)?;
        let tool = find_tool(&mapping, intent)?;
        let arguments = normalize_arguments(payload, &tool.input_schema);
        result = client.tools_call(&tool.mcp_name, &arguments)?;
    }
    normalize_result(&result)
}

/// Этап 3.3: `HassGetState` на ha-mcp. Цели разрешаются строго по полному
/// каталогу `ha_search` (`prepare_action`: семантические селекторы, bound,
/// fail-closed на malformed-записях — всё ДО любого чтения состояния);
/// прямой `entity_id` в payload отклонён ещё на входе. Состояние берётся
/// ТОЛЬКО из `ha_get_state` (по одному ID за вызов — форма ответа
/// `{data:{state,attributes}, metadata}` проверена на живом сервере),
/// а не из устаревшего поля `state` каталога. Ответ собирается в прежнем
/// конверте `query_answer` с семантическими именами; внутренние ID наружу
/// (включая диагностику ошибок) не отражаются.
fn execute_hamcp_get_state(client: &mut Client, payload: &Json) -> Result<Json, HaCliError> {
    // Разрешение всех целей до первого чтения состояния: prepare_action
    // рекурсивно отклоняет entity_id в payload до любой сети.
    let prepared = resolver::prepare_action(client, payload)?;
    let entities: Vec<Entity> = match &prepared.resolution {
        Resolution::Named(entity) => vec![entity.clone()],
        Resolution::Bulk { entities, .. } => entities.clone(),
    };
    // Любая ошибка после выбора целей (tools_call, stale-refresh, discovery,
    // разбор ответа) может содержать отражённые сервером внутренние ID —
    // одним и тем же scrub по ВСЕМ подготовленным целям, так как текст
    // может упоминать не только текущую.
    read_states_for_targets(client, &entities).map_err(|mut err| {
        err.message = scrub_internal_ids(&err.message, &entities);
        err
    })
}

/// Чтение состояний всех разрешённых целей: по одному `ha_get_state` на ID.
/// Кэш схем и stale-refresh разрешены: ha_get_state — read-only, повторный
/// вызов не может выполнить действие дважды.
fn read_states_for_targets(client: &mut Client, entities: &[Entity]) -> Result<Json, HaCliError> {
    let mut discovery = ToolDiscovery::new(None);
    let tool = discovery.get_tool(client, HA_GET_STATE_TOOL)?;
    let mut states = Vec::new();
    let mut speech_parts = Vec::new();
    for entity in entities {
        let arguments = json!({
            "entity_id": entity.entity_id.clone().unwrap_or_default(),
            "fields": ["state", "attributes"],
        });
        let mut result = client.tools_call(&tool.mcp_name, &arguments)?;
        if is_stale_tool_result(&result) {
            let mapping = discovery.refresh(client)?;
            let tool = find_tool(&mapping, HA_GET_STATE_TOOL)?;
            result = client.tools_call(&tool.mcp_name, &arguments)?;
        }
        let state = read_live_state(&result)?;
        // Порядок ключей важен для паритета вывода с Python-версией;
        // entity_id в вывод не попадает.
        let mut item = Map::new();
        item.insert("area".to_string(), Json::String(entity.area.clone()));
        item.insert("domain".to_string(), Json::String(entity.domain.clone()));
        item.insert("name".to_string(), Json::String(entity.name.clone()));
        item.insert("state".to_string(), Json::String(state.clone()));
        speech_parts.push(format!("{}: {}", entity.name, state));
        states.push(Json::Object(item));
    }
    Ok(json!({
        "ok": true,
        "response_type": "query_answer",
        "speech": speech_parts.join("; "),
        "data": {"states": states},
    }))
}

/// Разбор ответа `ha_get_state` для одного ID. Поддерживаются проверенная
/// форма `{data:{state, attributes}, metadata}` и возможный bulk-wrapper
/// (`states`/`errors`/`count`/`error_count`), если сервер вернёт его даже
/// на одиночный вызов: непустые `errors`/`error_count` распространяются как
/// ошибка, а `state` берётся только из единственного элемента `states`.
/// Отсутствие или нестроковость `state` — fail-closed ошибка, а не `unknown`.
fn read_live_state(result: &Json) -> Result<String, HaCliError> {
    let intent_error = |message: &str| HaCliError::new(ErrorType::Intent, message.to_string());
    if !result.is_object() {
        return Err(intent_error("unexpected tool result type"));
    }
    if truthy(result.get("isError")) {
        return Err(intent_error(&tool_error_message(
            result,
            "state request failed",
        )));
    }
    let structured = match result.get("structuredContent") {
        Some(value) if !value.is_null() => value.clone(),
        _ => {
            if !result.get("content").is_some_and(Json::is_array) {
                return Err(intent_error("tool result has no content"));
            }
            let text = extract_text(result);
            let parsed: Json = serde_json::from_str(&text)
                .map_err(|_| intent_error("state request returned text that is not valid JSON"))?;
            if !parsed.is_object() {
                return Err(intent_error("state request JSON is not an object"));
            }
            parsed
        }
    };
    if let Some(message) = success_failure_message(&structured, "state request reported failure") {
        return Err(intent_error(&message));
    }
    let errors_empty = structured
        .get("errors")
        .and_then(Json::as_array)
        .map(Vec::is_empty)
        .unwrap_or(true);
    let error_count = structured
        .get("error_count")
        .and_then(Json::as_i64)
        .unwrap_or(0);
    if !errors_empty || error_count > 0 {
        return Err(intent_error("state request reported errors for the target"));
    }
    let state = structured
        .get("data")
        .and_then(|data| data.get("state"))
        .and_then(Json::as_str)
        .or_else(|| {
            structured
                .get("states")
                .and_then(Json::as_array)
                .filter(|states| states.len() == 1)
                .and_then(|states| states[0].get("state"))
                .and_then(Json::as_str)
        })
        .filter(|state| !state.trim().is_empty());
    match state {
        Some(state) => Ok(state.to_string()),
        // Неполные данные не выдаются за результат: fail-closed.
        None => Err(intent_error(
            "state request returned no valid state for the target",
        )),
    }
}

/// Удаление внутренних ID из диагностического сообщения: каждый ID цели
/// заменяется семантическим именем. Сообщение сервера может отражать
/// переданный `entity_id` — наружу он не должен попадать.
fn scrub_internal_ids(message: &str, entities: &[Entity]) -> String {
    let mut message = message.to_string();
    for entity in entities {
        if let Some(id) = entity.entity_id.as_deref() {
            if !id.is_empty() {
                message = message.replace(id, &format!("'{}'", entity.name));
            }
        }
    }
    message
}

/// Перенос `normalize_result`.
pub fn normalize_result(result: &Json) -> Result<Json, HaCliError> {
    let intent_error = |message: &str| HaCliError::new(ErrorType::Intent, message.to_string());
    if !result.is_object() {
        return Err(intent_error("unexpected tool result type"));
    }
    // Ошибки инструмента проверяются ДО требования content: структурированная
    // ToolError или `{success:false}` без content — настоящая ошибка, а не
    // generic «нет content».
    if truthy(result.get("isError")) {
        return Err(intent_error(&tool_error_message(
            result,
            "intent execution failed",
        )));
    }
    if let Some(message) = result
        .get("structuredContent")
        .and_then(|value| success_failure_message(value, "tool reported failure"))
    {
        return Err(intent_error(&message));
    }
    // Нормальные результаты по-прежнему обязаны иметь content
    // (обратная совместимость с контрактом Assist).
    if !result.get("content").is_some_and(Json::is_array) {
        return Err(intent_error("tool result has no content"));
    }
    let text = extract_text(result);
    let intent_response = parse_intent_response(&text);
    if let Some(message) = success_failure_message(&intent_response, "tool reported failure") {
        return Err(intent_error(&message));
    }
    let speech_value = intent_response
        .get("speech")
        .cloned()
        .unwrap_or_else(|| Json::String(text));
    // Порядок ключей важен для паритета вывода с Python-версией
    // (serde_json с preserve_order сохраняет порядок вставки).
    let mut normalized = Map::new();
    normalized.insert("ok".to_string(), Json::Bool(true));
    normalized.insert(
        "response_type".to_string(),
        intent_response
            .get("response_type")
            .cloned()
            .unwrap_or_else(|| Json::String("action_done".to_string())),
    );
    normalized.insert(
        "speech".to_string(),
        Json::String(extract_speech(&speech_value)),
    );
    let structured = match result.get("structuredContent") {
        Some(value) => value.clone(),
        None => intent_response.get("data").cloned().unwrap_or(Json::Null),
    };
    if !structured.is_null() {
        normalized.insert("data".to_string(), structured);
    }
    Ok(Json::Object(normalized))
}

/// Перенос `_normalize_arguments`: строка оборачивается в массив,
/// если схема инструмента ожидает array для этого ключа.
pub fn normalize_arguments(payload: &Json, input_schema: &Json) -> Json {
    let properties = input_schema.get("properties").and_then(Json::as_object);
    let mut arguments = Map::new();
    if let Some(object) = payload.as_object() {
        for (key, value) in object {
            let expects_array = properties
                .and_then(|props| props.get(key))
                .and_then(|spec| spec.get("type"))
                .and_then(Json::as_str)
                == Some("array");
            let wrapped = matches!(value, Json::String(_)) && expects_array;
            arguments.insert(
                key.clone(),
                if wrapped {
                    json!([value])
                } else {
                    value.clone()
                },
            );
        }
    }
    Json::Object(arguments)
}

/// Перенос `_parse_intent_response`.
fn parse_intent_response(text: &str) -> Json {
    serde_json::from_str(text)
        .ok()
        .filter(Json::is_object)
        .unwrap_or_else(|| Json::Object(Map::new()))
}

/// Перенос `_extract_speech`.
fn extract_speech(speech: &Json) -> String {
    match speech {
        Json::String(text) => text.clone(),
        Json::Object(_) => speech
            .get("plain")
            .filter(|value| value.is_object())
            .and_then(|plain| plain.get("speech"))
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{HttpResponse, Transport};
    use crate::config::{Config, McpAuth};
    use crate::security::Secrets;
    use std::cell::RefCell;

    /// Транспорт, который «падает» при любом запросе: если после запрета
    /// entity_id произойдёт хоть один сетевой вызов, тест это увидит.
    struct NoNetwork;

    impl Transport for NoNetwork {
        fn post(
            &mut self,
            _url: &str,
            _payload: &Json,
            _headers: &[(String, String)],
        ) -> Result<HttpResponse, HaCliError> {
            panic!("no network request is allowed after payload validation")
        }
    }

    fn mcp_client() -> Client {
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        Client::new(config, Box::new(NoNetwork), &Secrets::new())
    }

    #[test]
    fn entity_id_in_payload_is_rejected_before_any_request() {
        let mut client = mcp_client();
        for payload in [
            json!({"entity_id": "light.kitchen"}),
            json!({"name": "light.kitchen"}),
            json!({"data": {"entity_ids": ["light.kitchen"]}}),
        ] {
            let err = execute(&mut client, "HassTurnOn", &payload).unwrap_err();
            assert!(matches!(err.kind, ErrorType::InvalidArguments));
        }
    }

    #[test]
    fn action_without_semantic_target_is_invalid_arguments() {
        let mut client = mcp_client();
        // Транспорт не вызывается: селекторы проверяются до сети.
        let err = execute(&mut client, "HassTurnOn", &json!({})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::InvalidArguments));
    }

    #[test]
    fn strict_intent_payload_validation_rejects_before_any_request() {
        // Этап 4.1: неизвестные ключи, плохие типы/диапазоны и недопустимые
        // сочетания отклоняются ДО сети (NoNetwork паникует при любом
        // запросе), включая prepare_action и ha_get_state.
        let mut client = mcp_client();
        for (intent, payload) in [
            (
                "HassLightSet",
                json!({"area": "kitchen", "brightness": 101}),
            ),
            // Дробный литерал — другой JSON-тип, отклоняется до сети.
            (
                "HassLightSet",
                json!({"area": "kitchen", "brightness": 50.0}),
            ),
            (
                "HassSetPosition",
                json!({"area": "kitchen", "position": 70.0}),
            ),
            (
                "HassLightSet",
                json!({"name": "Lamp", "color_temp_kelvin": "3000"}),
            ),
            (
                "HassLightSet",
                json!({"area": "kitchen", "color_temp_kelvin": 3000, "rgb_color": [1, 2, 3]}),
            ),
            (
                "HassLightSet",
                json!({"area": "kitchen", "domain": "switch"}),
            ),
            (
                "HassLightSet",
                json!({"area": "kitchen", "data": {"brightness": 50}}),
            ),
            (
                "HassSetPosition",
                json!({"area": "kitchen", "position": 150}),
            ),
            ("HassSetPosition", json!({"name": "Blinds"})),
            ("HassTurnOn", json!({"area": "kitchen", "brightness": 50})),
            (
                "HassGetState",
                json!({"area": "kitchen", "fields": ["state"]}),
            ),
            ("HassTurnOn", json!({"area": 123})),
        ] {
            let err = execute(&mut client, intent, &payload).unwrap_err();
            assert!(
                matches!(err.kind, ErrorType::InvalidArguments),
                "{intent} {payload}: {err}"
            );
        }
    }

    #[test]
    fn get_state_entity_id_in_payload_is_rejected_before_any_request() {
        let mut client = mcp_client();
        for payload in [
            json!({"entity_id": "light.kitchen"}),
            json!({"name": "light.kitchen"}),
            json!({"area": "Kitchen", "data": {"entity_ids": ["light.kitchen"]}}),
        ] {
            let err = execute(&mut client, "HassGetState", &payload).unwrap_err();
            assert!(matches!(err.kind, ErrorType::InvalidArguments));
        }
    }

    /// Скриптованный ha-mcp сервер для HassGetState: каталог из двух
    /// сущностей, состояние отдаёт `ha_get_state`; считает вызовы
    /// ha_search и ha_get_state.
    fn reply(id: u64, result: Json) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        }
    }

    /// Обёртка: Client владеет Box<dyn Transport>, а тесту нужен доступ
    /// к счётчикам того же экземпляра.
    struct SharedMock(std::rc::Rc<MockGetState>);

    impl Transport for SharedMock {
        fn post(
            &mut self,
            _url: &str,
            payload: &Json,
            _headers: &[(String, String)],
        ) -> Result<HttpResponse, HaCliError> {
            self.0.post(payload)
        }
    }

    struct MockGetState {
        state_reply: Json,
        search_calls: RefCell<usize>,
        get_state_calls: RefCell<usize>,
        /// На N-м вызове ha_get_state вернуть JSON-RPC error с отражённым
        /// внутренним ID в тексте.
        rpc_error_on: Option<usize>,
    }

    impl MockGetState {
        fn new(state_reply: Json) -> Self {
            Self {
                state_reply,
                search_calls: RefCell::new(0),
                get_state_calls: RefCell::new(0),
                rpc_error_on: None,
            }
        }

        fn with_rpc_error_on(state_reply: Json, call: usize) -> Self {
            Self {
                rpc_error_on: Some(call),
                ..Self::new(state_reply)
            }
        }

        fn search_count(&self) -> usize {
            *self.search_calls.borrow()
        }

        fn get_state_count(&self) -> usize {
            *self.get_state_calls.borrow()
        }

        fn post(&self, payload: &Json) -> Result<HttpResponse, HaCliError> {
            let id = payload.get("id").and_then(Json::as_u64).unwrap_or(0);
            match payload.get("method").and_then(Json::as_str) {
                Some("initialize") => Ok(reply(id, json!({"protocolVersion": "2025-03-26"}))),
                Some("notifications/initialized") => Ok(HttpResponse {
                    status: 202,
                    headers: Vec::new(),
                    body: String::new(),
                }),
                Some("tools/list") => Ok(reply(
                    id,
                    json!({"tools": [
                        {"name": "ha_get_overview", "inputSchema": {"type": "object"}},
                        {"name": "ha_search", "inputSchema": {"type": "object"}},
                        {"name": "ha_get_state", "inputSchema": {"type": "object"}},
                    ]}),
                )),
                Some("tools/call") => {
                    let name = payload
                        .pointer("/params/name")
                        .and_then(Json::as_str)
                        .unwrap_or("");
                    match name {
                        "ha_get_overview" => Ok(reply(
                            id,
                            json!({"structuredContent": {"domain_stats": {"light": 2}}}),
                        )),
                        "ha_search" => {
                            *self.search_calls.borrow_mut() += 1;
                            Ok(reply(
                                id,
                                json!({"structuredContent": {
                                    "entities": [
                                        {
                                            "entity_id": "light.a",
                                            "friendly_name": "One",
                                            "domain": "light",
                                            "state": "off",
                                            "area": "Kitchen",
                                            "aliases": [],
                                        },
                                        {
                                            "entity_id": "light.b",
                                            "friendly_name": "Two",
                                            "domain": "light",
                                            "state": "off",
                                            "area": "Kitchen",
                                            "aliases": [],
                                        },
                                    ],
                                    "entity_total_matches": 2,
                                    "partial": false,
                                    "errors": [],
                                    "entity_has_more": false,
                                    "entity_next_offset": null,
                                }}),
                            ))
                        }
                        "ha_get_state" => {
                            *self.get_state_calls.borrow_mut() += 1;
                            if self.rpc_error_on == Some(*self.get_state_calls.borrow()) {
                                // JSON-RPC error: текст отражает внутренние ID
                                // обеих целей каталога.
                                return Ok(HttpResponse {
                                    status: 200,
                                    headers: vec![(
                                        "Content-Type".to_string(),
                                        "application/json".to_string(),
                                    )],
                                    body: json!({
                                        "jsonrpc": "2.0",
                                        "id": id,
                                        "error": {
                                            "code": -32000,
                                            "message": "state lookup failed for light.a and light.b",
                                        },
                                    })
                                    .to_string(),
                                });
                            }
                            Ok(reply(id, self.state_reply.clone()))
                        }
                        other => panic!("unexpected tool call: {other}"),
                    }
                }
                other => panic!("unexpected method: {other:?}"),
            }
        }
    }

    fn get_state_client(mock: &std::rc::Rc<MockGetState>) -> Client {
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        Client::new(config, Box::new(SharedMock(mock.clone())), &Secrets::new())
    }

    fn state_payload(state: &str) -> Json {
        json!({"structuredContent": {
            "data": {"state": state, "attributes": {"friendly_name": "One"}},
            "metadata": {"entity_id": "light.a"},
        }})
    }

    #[test]
    fn get_state_single_target_reads_live_state() {
        let mock = std::rc::Rc::new(MockGetState::new(state_payload("on")));
        let mut client = get_state_client(&mock);
        let result = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap();
        assert_eq!(mock.search_count(), 1);
        assert_eq!(mock.get_state_count(), 1);
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["response_type"], json!("query_answer"));
        assert_eq!(result["speech"], json!("One: on"));
        let states = result["data"]["states"].as_array().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0]["name"], json!("One"));
        assert_eq!(states[0]["area"], json!("Kitchen"));
        assert_eq!(states[0]["domain"], json!("light"));
        assert_eq!(states[0]["state"], json!("on"));
        // Внутренний ID не отражается наружу.
        assert!(serde_json::to_string(&result)
            .unwrap()
            .find("light.a")
            .is_none());
    }

    #[test]
    fn get_state_bulk_reads_state_for_every_resolved_target() {
        let mock = std::rc::Rc::new(MockGetState::new(state_payload("on")));
        let mut client = get_state_client(&mock);
        let result = execute(&mut client, "HassGetState", &json!({"area": "Kitchen"})).unwrap();
        assert_eq!(mock.get_state_count(), 2);
        let states = result["data"]["states"].as_array().unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(result["speech"], json!("One: on; Two: on"));
        assert!(serde_json::to_string(&result)
            .unwrap()
            .find("light.b")
            .is_none());
    }

    #[test]
    fn get_state_uses_live_value_not_stale_catalog_state() {
        // Каталог говорит "off", ha_get_state — "on": берётся живое значение.
        let mock = std::rc::Rc::new(MockGetState::new(state_payload("unavailable")));
        let mut client = get_state_client(&mock);
        let result = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap();
        assert_eq!(result["data"]["states"][0]["state"], json!("unavailable"));
        assert_eq!(result["speech"], json!("One: unavailable"));
    }

    #[test]
    fn get_state_tool_error_is_propagated_and_scrubbed() {
        // isError=true, текст сервера отражает внутренний ID.
        let reply = json!({
            "isError": true,
            "content": [{"type": "text", "text": "entity light.a not found"}],
        });
        let mock = std::rc::Rc::new(MockGetState::new(reply));
        let mut client = get_state_client(&mock);
        let err = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert!(!err.message.contains("light.a"));
        assert!(err.message.contains("One"));
    }

    #[test]
    fn get_state_structured_failure_is_propagated() {
        let reply = json!({"structuredContent": {"success": false, "error": "target is gone"}});
        let mock = std::rc::Rc::new(MockGetState::new(reply));
        let mut client = get_state_client(&mock);
        let err = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
    }

    #[test]
    fn get_state_jsonrpc_error_is_scrubbed_before_propagation() {
        // JSON-RPC error от ha_get_state отражает внутренний ID: он не
        // должен попасть в сообщение об ошибке.
        let mock = std::rc::Rc::new(MockGetState::with_rpc_error_on(state_payload("on"), 1));
        let mut client = get_state_client(&mock);
        let err = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::HaApi));
        // Вычищается ID подготовленной цели; light.b в single-набор не входит.
        assert!(!err.message.contains("light.a"));
    }

    #[test]
    fn get_state_jsonrpc_error_on_later_bulk_target_scrubs_all_ids() {
        // Ошибка на второй цели bulk-набора: текст может упоминать любой
        // из разрешённых ID — вычищаются все.
        let mock = std::rc::Rc::new(MockGetState::with_rpc_error_on(state_payload("on"), 2));
        let mut client = get_state_client(&mock);
        let err = execute(&mut client, "HassGetState", &json!({"area": "Kitchen"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::HaApi));
        assert!(!err.message.contains("light.a"));
        assert!(!err.message.contains("light.b"));
        assert_eq!(mock.get_state_count(), 2);
    }

    #[test]
    fn get_state_missing_state_is_fail_closed() {
        for reply in [
            json!({"structuredContent": {"data": {"attributes": {}}, "metadata": {}}}),
            json!({"structuredContent": {"data": {"state": 42}, "metadata": {}}}),
            json!({"structuredContent": {"data": {"state": ""}, "metadata": {}}}),
            json!({"structuredContent": {}}),
            json!({"structuredContent": {"states": [], "count": 0, "errors": [], "error_count": 0}}),
        ] {
            let mock = std::rc::Rc::new(MockGetState::new(reply));
            let mut client = get_state_client(&mock);
            let err = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap_err();
            assert!(matches!(err.kind, ErrorType::Intent));
            assert!(err.message.contains("state request"));
            // ID из metadata-ответов и сообщений не утекает.
            assert!(!err.message.contains("light.a"));
        }
    }

    #[test]
    fn get_state_bulk_wrapper_errors_are_propagated() {
        let reply = json!({"structuredContent": {
            "states": [], "count": 0, "errors": ["failed"], "error_count": 1,
        }});
        let mock = std::rc::Rc::new(MockGetState::new(reply));
        let mut client = get_state_client(&mock);
        let err = execute(&mut client, "HassGetState", &json!({"area": "Kitchen"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
    }

    #[test]
    fn get_state_unknown_and_ambiguous_targets_fail_before_any_read() {
        for payload in [
            json!({"name": "Missing Lamp"}),
            json!({"name": "One", "domain": "switch"}),
            json!({"area": "Attic"}),
        ] {
            let mock = std::rc::Rc::new(MockGetState::new(state_payload("on")));
            let mut client = get_state_client(&mock);
            let err = execute(&mut client, "HassGetState", &payload).unwrap_err();
            assert!(matches!(err.kind, ErrorType::Intent));
            assert_eq!(mock.get_state_count(), 0, "no state read for {payload}");
        }
    }

    #[test]
    fn get_state_without_mcp_url_keeps_assist_fallback_path() {
        // Assist endpoint: HassGetState не найден в tools/list → прежний
        // fallback query_state по live context (без ha_get_state).
        struct AssistMock;
        impl Transport for AssistMock {
            fn post(
                &mut self,
                _url: &str,
                payload: &Json,
                _headers: &[(String, String)],
            ) -> Result<HttpResponse, HaCliError> {
                let id = payload.get("id").and_then(Json::as_u64).unwrap_or(0);
                match payload.get("method").and_then(Json::as_str) {
                    Some("initialize") => Ok(reply(id, json!({"protocolVersion": "1.0"}))),
                    Some("notifications/initialized") => Ok(HttpResponse {
                        status: 202,
                        headers: Vec::new(),
                        body: String::new(),
                    }),
                    Some("tools/list") => {
                        Ok(reply(id, json!({"tools": [{"name": "GetLiveContext"}]})))
                    }
                    Some("tools/call") => {
                        let name = payload
                            .pointer("/params/name")
                            .and_then(Json::as_str)
                            .unwrap_or("");
                        assert_eq!(name, "GetLiveContext");
                        Ok(reply(
                            id,
                            json!({"content": [{"type": "text", "text": json!({"success": true, "result": "Live Context:\n- names: One\n  areas: Kitchen\n  domain: light\n  state: off"}).to_string()}]}),
                        ))
                    }
                    other => panic!("unexpected method: {other:?}"),
                }
            }
        }
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: None,
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        let mut client = Client::new(config, Box::new(AssistMock), &Secrets::new());
        let result = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap();
        assert_eq!(result["response_type"], json!("query_answer"));
        assert_eq!(result["data"]["states"][0]["state"], json!("off"));
    }
}
