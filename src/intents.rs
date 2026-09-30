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

/// Инструмент ha-mcp для вызова конкретного сервиса (этап 4.2). CLI
/// передаёт сюда только одиночный внутренний ID, полученный из полного
/// каталога `ha_search` через `resolver::prepare_action`; домен и сервис
/// задаются самим CLI по интенту, никогда — пользовательским payload.
pub const HA_CALL_SERVICE_TOOL: &str = "ha_call_service";

/// Явный allowlist доменов, для которых `HassTurnOn` / `HassTurnOff`
/// отображаются на `domain.turn_on` / `domain.turn_off`. Домены вне списка
/// не вызываются, даже если сущность есть в отфильтрованном каталоге:
/// административные и иные домены через интент недоступны. Расширение
/// списка — осознанное изменение кода, а не поведение сервера.
pub const TURN_SERVICE_DOMAINS: &[&str] = &[
    "light",
    "switch",
    "fan",
    "cover",
    "media_player",
    "humidifier",
    "water_heater",
    "siren",
    "vacuum",
];

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
        // Этап 4.2: действия отображаются на `ha_call_service`. Полный
        // набор целей разрешается ДО первого вызова сервиса
        // (`prepare_action`: рекурсивный запрет entity_id, семантические
        // селекторы, bound, fail-closed на malformed-записях); домен каждой
        // цели проверяется по интенту; затем — по одному
        // `ha_call_service` на цель с одиночным внутренним ID.
        let prepared = resolver::prepare_action(client, payload)?;
        return execute_hamcp_action(client, intent, payload, &prepared);
    }
    let mut discovery = ToolDiscovery::for_endpoint(client.config.mcp_url.as_deref());
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
    let mut discovery = ToolDiscovery::for_endpoint(client.config.mcp_url.as_deref());
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

/// Этап 4.2: действия `HassTurnOn` / `HassTurnOff` / `HassLightSet` /
/// `HassSetPosition` на ha-mcp. Набор целей уже разрешён полностью
/// (`prepared`), пользовательский entity_id отклонён до сети, ID целей —
/// только из каталога `ha_search`. Домен каждой цели проверяется по
/// интенту ДО первого вызова сервиса. Затем — ровно один
/// `ha_call_service` на цель (одиночный ID; параметр `wait` ha-mcp
/// подтверждает изменение только одиночного `entity_id`, поэтому
/// comma-separated массовый вызов не используется никогда). Вызов записи
/// после отправки не повторяется: timeout/stale/ошибка ответа не
/// доказывают, что сервис не выполнился.
fn execute_hamcp_action(
    client: &mut Client,
    intent: &str,
    payload: &Json,
    prepared: &resolver::Prepared,
) -> Result<Json, HaCliError> {
    let entities: Vec<Entity> = match &prepared.resolution {
        Resolution::Named(entity) => vec![entity.clone()],
        Resolution::Bulk { entities, .. } => entities.clone(),
    };
    // Проверка доменов по цели — после разрешения, до любого вызова
    // сервиса. Для `HassLightSet` / `HassSetPosition` домен обязан быть
    // `light` / `cover` даже если селектор `domain` в payload отсутствовал
    // (массовый выбор по area не должен затронуть чужой домен).
    // Агрегаты блокируются ДО первого вызова: сущность, помеченная
    // ha_search `is_group=true` (или с нечитаемым флагом), а также домен
    // `group` в любой цели действия отклоняются явно — фильтрация групп
    // никогда не выполняется молча. Read-only `HassGetState` и `context`
    // эту проверку не применяют (старая семантика сохранена).
    for entity in &entities {
        check_action_entity(intent, entity)?;
    }
    let plan = service_plan(intent, payload, &entities[0])?;
    // Поиск инструмента до первого вызова; кэш схем — только read-only
    // discovery, повторной отправки действия он не вызывает.
    let mut discovery = ToolDiscovery::for_endpoint(client.config.mcp_url.as_deref());
    let tool = discovery.get_tool(client, HA_CALL_SERVICE_TOOL)?;
    for (performed, entity) in entities.iter().enumerate() {
        // Имя домена для TurnOn/Off — домен самой цели (проверен
        // allowlist'ом выше); для LightSet/SetPosition — фиксированный.
        let domain = plan.domain.as_deref().unwrap_or(&entity.domain);
        let mut arguments = json!({
            "domain": domain,
            "service": plan.service,
            "entity_id": entity.entity_id.clone().unwrap_or_default(),
        });
        if plan.data.as_object().is_some_and(|data| !data.is_empty()) {
            arguments["data"] = plan.data.clone();
        }
        let result = client.tools_call(&tool.mcp_name, &arguments);
        match result {
            Ok(result) => {
                if is_stale_tool_result(&result) {
                    // stale tool name: вызов уже отправлен и мог
                    // выполниться — повтор запрещён.
                    return Err(action_error(
                        performed,
                        entities.len(),
                        &format!(
                            "tool name changed after the call was sent; the action \
                             may still have been performed for '{}' and was not repeated",
                            entity.name
                        ),
                        &entities,
                    ));
                }
                if let Err(message) = normalize_action_result(&result) {
                    return Err(action_error(performed, entities.len(), &message, &entities));
                }
            }
            Err(err) => {
                // Ошибка транспорта/сервера на вызове записи: повтор
                // запрещён, факт выполнения неизвестен. Текст НЕ
                // утверждает, что запрос был отправлен: при DNS-fail или
                // отказе соединения запрос мог не покинуть клиента, а
                // JSON-RPC error (например, «unknown tool») означает, что
                // запрос дошёл, но сервис не выполнялся — клиент не может
                // это различить, поэтому формулировка нейтральна.
                return Err(action_error(
                    performed,
                    entities.len(),
                    &format!(
                        "{}; whether the request reached the server is unknown, \
                         the action may still have been performed, and the call \
                         was not repeated automatically",
                        err.message
                    ),
                    &entities,
                ));
            }
        }
    }
    let names: Vec<String> = entities.iter().map(|e| e.name.clone()).collect();
    let mut envelope = Map::new();
    envelope.insert("ok".to_string(), Json::Bool(true));
    envelope.insert(
        "response_type".to_string(),
        Json::String("action_done".to_string()),
    );
    envelope.insert(
        "speech".to_string(),
        Json::String(format!("{}: {}", plan.summary_verb, names.join(", "))),
    );
    Ok(Json::Object(envelope))
}

/// Сообщение об ошибке после частичного выполнения: не утверждает, что
/// оставшиеся цели не выполнились, называет число успешно выполненных
/// вызовов и вычищает внутренние ID из текста сервера.
fn action_error(performed: usize, total: usize, reason: &str, entities: &[Entity]) -> HaCliError {
    let message = format!(
        "action was performed for {performed} of {total} targets before failing: \
         {reason}; already performed calls are not rolled back and the \
         remaining targets were not attempted again"
    );
    HaCliError::new(ErrorType::Intent, scrub_internal_ids(&message, entities))
}

/// Проверка одной цели действия до первого вызова сервиса: явные
/// агрегаты (`ha_search.is_group=true` или нечитаемый флаг) и домен
/// `group` отклоняются с ошибкой — группа никогда не отфильтровывается
/// молча и не получает сервисный вызов, эффект действия обязан остаться
/// в пределах отфильтрованного набора листьев. Далее — домен по интенту
/// (`check_action_domain`).
fn check_action_entity(intent: &str, entity: &Entity) -> Result<(), HaCliError> {
    if entity.domain.eq_ignore_ascii_case("group") {
        return Err(HaCliError::new(
            ErrorType::Intent,
            format!(
                "domain 'group' is not a valid action target ('{}' is an \
                 aggregate); no service was called",
                entity.name
            ),
        ));
    }
    if entity.is_group {
        return Err(HaCliError::new(
            ErrorType::Intent,
            format!(
                "'{}' is a group or aggregate entity (or has an unreadable \
                 is_group flag); actions target leaf entities only, so no \
                 service was called",
                entity.name
            ),
        ));
    }
    check_action_domain(intent, &entity.domain)
}

/// Проверка домена одной цели по интенту. `HassTurnOn`/`HassTurnOff` —
/// явный allowlist; `HassLightSet` — только `light`; `HassSetPosition` —
/// только `cover`. Иначе вызов сервиса не выполняется вовсе.
fn check_action_domain(intent: &str, domain: &str) -> Result<(), HaCliError> {
    let ok = match intent {
        "HassLightSet" => domain.eq_ignore_ascii_case("light"),
        "HassSetPosition" => domain.eq_ignore_ascii_case("cover"),
        "HassTurnOn" | "HassTurnOff" => TURN_SERVICE_DOMAINS
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain)),
        other => {
            return Err(HaCliError::new(
                ErrorType::InvalidArguments,
                format!("unknown or blocked intent: {other}"),
            ))
        }
    };
    if ok {
        return Ok(());
    }
    Err(HaCliError::new(
        ErrorType::Intent,
        format!(
            "domain '{domain}' is not supported for {intent} via ha-mcp; \
                 no service was called"
        ),
    ))
}

/// План вызова сервиса для интента: домен (если фиксирован), имя сервиса,
/// валидированные сервисные данные (только allowlist-поля, отображение
/// `brightness` → `brightness_pct`) и глагол для speech.
struct ActionPlan {
    domain: Option<String>,
    service: &'static str,
    data: Json,
    summary_verb: String,
}

fn service_plan(intent: &str, payload: &Json, _first: &Entity) -> Result<ActionPlan, HaCliError> {
    let field = |key: &str| payload.get(key).cloned();
    let plan = match intent {
        "HassTurnOn" => ActionPlan {
            domain: None,
            service: "turn_on",
            data: Json::Null,
            summary_verb: "Turned on".to_string(),
        },
        "HassTurnOff" => ActionPlan {
            domain: None,
            service: "turn_off",
            data: Json::Null,
            summary_verb: "Turned off".to_string(),
        },
        "HassLightSet" => {
            // Только validated fields (этап 4.1); произвольные ключи
            // payload в data не попадают.
            let mut data = Map::new();
            if let Some(brightness) = field("brightness").and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
            }) {
                // Семантика скилла: 0..100 percent → подтверждённое поле
                // `brightness_pct` сервиса light.turn_on (ha_list_services,
                // этап 1).
                data.insert(
                    "brightness_pct".to_string(),
                    Json::Number(brightness.into()),
                );
            }
            if let Some(kelvin) = field("color_temp_kelvin").and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
            }) {
                data.insert("color_temp_kelvin".to_string(), Json::Number(kelvin.into()));
            }
            if let Some(transition) = field("transition") {
                data.insert("transition".to_string(), transition);
            }
            if let Some(rgb) = field("rgb_color") {
                data.insert("rgb_color".to_string(), rgb);
            }
            if let Some(effect) = field("effect") {
                data.insert("effect".to_string(), effect);
            }
            ActionPlan {
                domain: Some("light".to_string()),
                service: "turn_on",
                data: Json::Object(data),
                summary_verb: "Set light".to_string(),
            }
        }
        "HassSetPosition" => {
            // cover.set_cover_position: `data.position` — действующий
            // контракт сервиса Home Assistant (перепроверен по актуальной
            // документации ha-mcp `ha_call_service` / сервисов HA; живой
            // сервер для проверки не использовался).
            let position = field("position")
                .and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
                })
                .ok_or_else(|| {
                    HaCliError::new(
                        ErrorType::Intent,
                        "position is required for HassSetPosition",
                    )
                })?;
            ActionPlan {
                domain: Some("cover".to_string()),
                service: "set_cover_position",
                data: json!({"position": position}),
                summary_verb: "Set position".to_string(),
            }
        }
        other => {
            return Err(HaCliError::new(
                ErrorType::InvalidArguments,
                format!("unknown or blocked intent: {other}"),
            ))
        }
    };
    Ok(plan)
}

/// Нормализация результата одного `ha_call_service`: `isError`,
/// `{success:false}`, `partial:true` и непустые `errors`/`warnings` —
/// ошибки; успешный результат не обязан иметь content.
fn normalize_action_result(result: &Json) -> Result<(), String> {
    if !result.is_object() {
        return Err("unexpected tool result type".to_string());
    }
    if truthy(result.get("isError")) {
        return Err(tool_error_message(result, "service call failed"));
    }
    let structured = match result.get("structuredContent") {
        Some(value) if !value.is_null() => value.clone(),
        _ => {
            if !result.get("content").is_some_and(Json::is_array) {
                return Err("tool result has no content".to_string());
            }
            let text = extract_text(result);
            match serde_json::from_str::<Json>(&text) {
                Ok(parsed) if parsed.is_object() => parsed,
                _ => {
                    // Текст без JSON не означает ошибку сервиса: пустой
                    // объект не даёт ложных partial/warnings.
                    Json::Object(Map::new())
                }
            }
        }
    };
    if let Some(message) = success_failure_message(&structured, "service call reported failure") {
        return Err(message);
    }
    let has_errors = !structured
        .get("errors")
        .and_then(Json::as_array)
        .map(Vec::is_empty)
        .unwrap_or(true);
    let partial = truthy(structured.get("partial"));
    let has_warnings = !structured
        .get("warnings")
        .and_then(Json::as_array)
        .map(Vec::is_empty)
        .unwrap_or(true);
    if has_errors || partial || has_warnings {
        return Err("service call reported partial result or warnings".to_string());
    }
    Ok(())
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

    /// Этап 4.2: скриптованный ha-mcp сервер для действий. Каталог
    /// (light.a/light.b/switch.c/automation.d в Kitchen), вызовы
    /// `ha_call_service` записываются для проверок домена/сервиса/data и
    /// числа вызовов; настраиваемая ошибка на N-м вызове.
    fn entity_raw(entity_id: &str, area: &str, name: &str) -> Json {
        json!({
            "entity_id": entity_id,
            "friendly_name": name,
            "domain": entity_id.split('.').next().unwrap_or("unknown"),
            "state": "off",
            "area": area,
            "aliases": [],
        })
    }

    struct MockAction {
        fail_on: Option<usize>,
        fail_body: Json,
        transport_error_on: Option<usize>,
        calls: RefCell<Vec<Json>>,
        /// Включить в каталог fixture'ы агрегатов: light.group
        /// (`is_group=true`) и group.all (домен `group`).
        include_groups: bool,
    }

    impl MockAction {
        fn new() -> Self {
            Self {
                fail_on: None,
                fail_body: json!({"isError": true, "content": [
                    {"type": "text", "text": "service refused for light.b"}
                ]}),
                transport_error_on: None,
                calls: RefCell::new(Vec::new()),
                include_groups: false,
            }
        }

        fn with_groups() -> Self {
            Self {
                include_groups: true,
                ..Self::new()
            }
        }

        fn catalog_entities(&self) -> Vec<Json> {
            let mut all = vec![
                entity_raw("light.a", "Kitchen", "One"),
                entity_raw("light.b", "Kitchen", "Two"),
                entity_raw("switch.c", "Kitchen", "Three"),
                entity_raw("automation.d", "Kitchen", "Routine"),
                entity_raw("cover.e", "Kitchen", "Blinds"),
            ];
            if self.include_groups {
                let mut group = entity_raw("light.group_kitchen", "Kitchen", "Group Lights");
                group["is_group"] = json!(true);
                all.push(group);
                let mut aggregate = entity_raw("group.all_lights", "Kitchen", "All Lights");
                aggregate["is_group"] = json!(true);
                all.push(aggregate);
                // Нечитаемый флаг: не булево значение — fail-closed.
                let mut unreadable = entity_raw("light.flag_odd", "Kitchen", "Odd Flag");
                unreadable["is_group"] = json!("yes");
                all.push(unreadable);
            }
            all
        }

        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }

        fn calls(&self) -> std::cell::Ref<'_, Vec<Json>> {
            self.calls.borrow()
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
                        {"name": "ha_call_service", "inputSchema": {"type": "object"}},
                    ]}),
                )),
                Some("tools/call") => {
                    let name = payload
                        .pointer("/params/name")
                        .and_then(Json::as_str)
                        .unwrap_or("");
                    match name {
                        "ha_get_overview" => {
                            let mut stats = json!({
                                "light": 2, "switch": 1, "automation": 1, "cover": 1,
                            });
                            if self.include_groups {
                                stats["group"] = json!(1);
                            }
                            Ok(reply(
                                id,
                                json!({"structuredContent": {"domain_stats": stats}}),
                            ))
                        }
                        "ha_search" => {
                            // Ответ respect'ит domain_filter, как живой
                            // сервер: total соответствует отфильтрованному
                            // набору (иначе каталог признаётся неполным).
                            let filter = payload
                                .pointer("/params/arguments/domain_filter")
                                .and_then(Json::as_str)
                                .unwrap_or("");
                            let matched: Vec<Json> = self
                                .catalog_entities()
                                .into_iter()
                                .filter(|entity| {
                                    filter.is_empty()
                                        || entity["entity_id"]
                                            .as_str()
                                            .unwrap_or("")
                                            .starts_with(&format!("{filter}."))
                                })
                                .collect();
                            let total = matched.len();
                            Ok(reply(
                                id,
                                json!({"structuredContent": {
                                    "entities": matched,
                                    "entity_total_matches": total,
                                    "partial": false,
                                    "errors": [],
                                    "entity_has_more": false,
                                    "entity_next_offset": null,
                                }}),
                            ))
                        }
                        "ha_call_service" => {
                            let index = self.call_count() + 1;
                            self.calls
                                .borrow_mut()
                                .push(payload["params"]["arguments"].clone());
                            if self.transport_error_on == Some(index) {
                                return Err(HaCliError::new(
                                    ErrorType::Connection,
                                    "connection lost while waiting for the response",
                                ));
                            }
                            if self.fail_on == Some(index) {
                                return Ok(reply(id, self.fail_body.clone()));
                            }
                            Ok(reply(id, json!({"structuredContent": {"success": true}})))
                        }
                        other => panic!("unexpected tool call: {other}"),
                    }
                }
                other => panic!("unexpected method: {other:?}"),
            }
        }
    }

    fn action_client(mock: &std::rc::Rc<MockAction>) -> Client {
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        Client::new(
            config,
            Box::new(ActionShared(mock.clone())),
            &Secrets::new(),
        )
    }

    /// Обёртка для MockAction (аналог SharedMock для MockGetState).
    struct ActionShared(std::rc::Rc<MockAction>);

    impl Transport for ActionShared {
        fn post(
            &mut self,
            _url: &str,
            payload: &Json,
            _headers: &[(String, String)],
        ) -> Result<HttpResponse, HaCliError> {
            self.0.post(payload)
        }
    }

    #[test]
    fn turn_on_single_target_calls_service_with_resolved_id_once() {
        let mock = std::rc::Rc::new(MockAction::new());
        let mut client = action_client(&mock);
        let result = execute(&mut client, "HassTurnOn", &json!({"name": "One"})).unwrap();
        assert_eq!(mock.call_count(), 1);
        let call = &mock.calls()[0];
        assert_eq!(call["domain"], json!("light"));
        assert_eq!(call["service"], json!("turn_on"));
        assert_eq!(call["entity_id"], json!("light.a"));
        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["response_type"], json!("action_done"));
        assert!(result["speech"].as_str().unwrap().contains("One"));
        // Внутренний ID наружу не попадает.
        assert!(serde_json::to_string(&result)
            .unwrap()
            .find("light.a")
            .is_none());
    }

    #[test]
    fn turn_on_bulk_calls_service_once_per_target_with_own_domain() {
        // Никогда comma-separated: по одному вызову на цель.
        let mock = std::rc::Rc::new(MockAction::new());
        let mut client = action_client(&mock);
        let result = execute(
            &mut client,
            "HassTurnOff",
            &json!({"area": "Kitchen", "domain": "light"}),
        )
        .unwrap();
        assert_eq!(result["response_type"], json!("action_done"));
        let calls = mock.calls();
        assert_eq!(calls.len(), 2);
        for (call, expected) in calls.iter().zip(["light.a", "light.b"]) {
            assert_eq!(call["service"], json!("turn_off"));
            assert_eq!(call["entity_id"], json!(expected));
            let id = call["entity_id"].as_str().unwrap();
            assert!(!id.contains(','), "no comma-separated targets");
        }
        assert!(calls[0].get("data").is_none() || calls[0]["data"].is_null());
    }

    #[test]
    fn light_set_maps_validated_fields_to_light_turn_on_data() {
        let mock = std::rc::Rc::new(MockAction::new());
        let mut client = action_client(&mock);
        execute(
            &mut client,
            "HassLightSet",
            &json!({"name": "One", "brightness": 50, "color_temp_kelvin": 3000, "transition": 1.5}),
        )
        .unwrap();
        let call = &mock.calls()[0];
        assert_eq!(call["domain"], json!("light"));
        assert_eq!(call["service"], json!("turn_on"));
        assert_eq!(call["data"]["brightness_pct"], json!(50));
        assert_eq!(call["data"]["color_temp_kelvin"], json!(3000));
        assert_eq!(call["data"]["transition"], json!(1.5));
        // Только validated fields; произвольных ключей нет.
        let data_keys: Vec<&str> = call["data"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            data_keys,
            vec!["brightness_pct", "color_temp_kelvin", "transition"]
        );
    }

    #[test]
    fn set_position_calls_cover_set_cover_position_with_position() {
        let mock = std::rc::Rc::new(MockAction::new());
        let mut client = action_client(&mock);
        execute(
            &mut client,
            "HassSetPosition",
            &json!({"name": "Blinds", "position": 70}),
        )
        .unwrap();
        let call = &mock.calls()[0];
        assert_eq!(call["domain"], json!("cover"));
        assert_eq!(call["service"], json!("set_cover_position"));
        assert_eq!(call["data"]["position"], json!(70));
    }

    #[test]
    fn light_set_rejects_wrong_domain_target_even_without_domain_selector() {
        // Каталог Kitchen содержит switch и automation: массовый
        // HassLightSet по area обязан упасть ДО первого вызова сервиса,
        // хотя селектор domain в payload отсутствовал.
        let mock = std::rc::Rc::new(MockAction::new());
        let mut client = action_client(&mock);
        let err = execute(
            &mut client,
            "HassLightSet",
            &json!({"area": "Kitchen", "brightness": 50}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert_eq!(mock.call_count(), 0);
        let err = execute(
            &mut client,
            "HassSetPosition",
            &json!({"area": "Kitchen", "position": 50}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert_eq!(mock.call_count(), 0);
    }

    #[test]
    fn turn_on_rejects_domains_outside_allowlist() {
        let mock = std::rc::Rc::new(MockAction::new());
        let mut client = action_client(&mock);
        let err = execute(
            &mut client,
            "HassTurnOn",
            &json!({"domain": "automation", "area": "Kitchen"}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert!(err.message.contains("automation"));
        assert_eq!(mock.call_count(), 0);
    }

    #[test]
    fn failure_on_later_target_reports_partial_performance_without_retry() {
        // M=2, второй сбой: первый уже выполнен; повторных вызовов нет,
        // сообщение не утверждает, что остальные не выполнились, и не
        // раскрывает внутренние ID.
        let mut mock_impl = MockAction::new();
        mock_impl.fail_on = Some(2);
        let mock = std::rc::Rc::new(mock_impl);
        let mut client = action_client(&mock);
        let err = execute(
            &mut client,
            "HassTurnOn",
            &json!({"area": "Kitchen", "domain": "light"}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        let message = err.message;
        assert!(
            message.contains("performed for 1 of 2 targets"),
            "{message}"
        );
        assert!(!message.contains("was not performed for"));
        assert!(!message.contains("light.a"));
        assert!(!message.contains("light.b"));
        assert!(message.contains("Two"));
        assert_eq!(mock.call_count(), 2, "no retry after a failed write call");
    }

    #[test]
    fn transport_error_after_send_is_not_retried_and_reports_uncertainty() {
        let mut mock_impl = MockAction::new();
        mock_impl.transport_error_on = Some(1);
        let mock = std::rc::Rc::new(mock_impl);
        let mut client = action_client(&mock);
        let err = execute(
            &mut client,
            "HassTurnOn",
            &json!({"area": "Kitchen", "domain": "light"}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert!(err.message.contains("may still have been performed"));
        assert!(err.message.contains("not repeated"));
        // Вызов был отправлен один раз и не повторялся.
        assert_eq!(mock.call_count(), 1);
    }

    #[test]
    fn transport_error_text_does_not_claim_the_request_was_sent() {
        // Ошибка до отправки (DNS/connect fail): клиент не может знать,
        // покинул ли запрос машину, поэтому текст ошибки не утверждает
        // «sent» ни в какой форме — но повтор по-прежнему не выполняется,
        // а неопределённость исхода сохраняется.
        struct ConnectFailOnWrite(std::rc::Rc<MockAction>);
        impl Transport for ConnectFailOnWrite {
            fn post(
                &mut self,
                _url: &str,
                payload: &Json,
                _headers: &[(String, String)],
            ) -> Result<HttpResponse, HaCliError> {
                if payload.get("method").and_then(Json::as_str) == Some("tools/call")
                    && payload.pointer("/params/name").and_then(Json::as_str)
                        == Some("ha_call_service")
                {
                    self.0
                        .calls
                        .borrow_mut()
                        .push(payload["params"]["arguments"].clone());
                    return Err(HaCliError::new(
                        ErrorType::Connection,
                        "Unable to connect to Home Assistant",
                    ));
                }
                self.0.post(payload)
            }
        }
        let mock = std::rc::Rc::new(MockAction::new());
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        let mut client = Client::new(
            config,
            Box::new(ConnectFailOnWrite(mock.clone())),
            &Secrets::new(),
        );
        let err = execute(&mut client, "HassTurnOn", &json!({"name": "One"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        let lowered = err.message.to_lowercase();
        assert!(!lowered.contains("sent"), "{err}");
        assert!(
            err.message.contains("may still have been performed"),
            "{err}"
        );
        assert!(err.message.contains("not repeated"), "{err}");
    }

    #[test]
    fn stale_tool_name_as_jsonrpc_error_is_not_retried() {
        // Тот же stale-случай, но сервер отвечает JSON-RPC error
        // («unknown tool») вместо MCP isError: повтор так же запрещён,
        // исход помечен как неизвестный.
        struct RpcErrorMock(std::rc::Rc<MockAction>);
        impl Transport for RpcErrorMock {
            fn post(
                &mut self,
                _url: &str,
                payload: &Json,
                _headers: &[(String, String)],
            ) -> Result<HttpResponse, HaCliError> {
                if payload.get("method").and_then(Json::as_str) == Some("tools/call")
                    && payload.pointer("/params/name").and_then(Json::as_str)
                        == Some("ha_call_service")
                {
                    self.0
                        .calls
                        .borrow_mut()
                        .push(payload["params"]["arguments"].clone());
                    let id = payload.get("id").and_then(Json::as_u64).unwrap_or(0);
                    return Ok(HttpResponse {
                        status: 200,
                        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                        body: json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32602,
                                "message": "Tool ha_call_service not found",
                            },
                        })
                        .to_string(),
                    });
                }
                self.0.post(payload)
            }
        }
        let mock = std::rc::Rc::new(MockAction::new());
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        let mut client = Client::new(
            config,
            Box::new(RpcErrorMock(mock.clone())),
            &Secrets::new(),
        );
        let err = execute(&mut client, "HassTurnOn", &json!({"name": "One"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent), "{err}");
        assert!(
            err.message.contains("may still have been performed"),
            "{err}"
        );
        assert!(err.message.contains("not repeated"), "{err}");
        assert_eq!(mock.call_count(), 1, "no second write after JSON-RPC error");
    }

    #[test]
    fn payload_cannot_select_tool_service_or_admin_data() {
        // Пользовательский payload не может выбрать инструмент,
        // сервис или вложенные admin-данные: ключи вне allowlist
        // отклоняются ДО любых сетевых вызовов (NoNetwork паникует).
        let mut client = mcp_client();
        for payload in [
            json!({"area": "kitchen", "service": "restart"}),
            json!({"area": "kitchen", "tool": "ha_call_service"}),
            json!({"area": "kitchen", "domain": "light", "service": "turn_on"}),
            json!({"area": "kitchen", "data": {"admin": true}}),
            json!({"area": "kitchen", "name": "One", "wait": true}),
        ] {
            let err = execute(&mut client, "HassTurnOn", &payload).unwrap_err();
            assert!(
                matches!(err.kind, ErrorType::InvalidArguments),
                "{payload}: {err}"
            );
        }
    }

    #[test]
    fn stale_tool_name_after_write_call_is_not_retried() {
        struct StaleMock(std::rc::Rc<MockAction>);
        impl Transport for StaleMock {
            fn post(
                &mut self,
                _url: &str,
                payload: &Json,
                _headers: &[(String, String)],
            ) -> Result<HttpResponse, HaCliError> {
                // tools/call с ha_call_service → stale-ответ (имя
                // изменилось после отправки; вызов мог выполниться).
                if payload.get("method").and_then(Json::as_str) == Some("tools/call")
                    && payload.pointer("/params/name").and_then(Json::as_str)
                        == Some("ha_call_service")
                {
                    self.0
                        .calls
                        .borrow_mut()
                        .push(payload["params"]["arguments"].clone());
                    let id = payload.get("id").and_then(Json::as_u64).unwrap_or(0);
                    return Ok(HttpResponse {
                        status: 200,
                        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                        body: json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"isError": true, "content": [
                                {"type": "text", "text": "Tool ha_call_service not found"}
                            ]},
                        })
                        .to_string(),
                    });
                }
                self.0.post(payload)
            }
        }
        let mock = std::rc::Rc::new(MockAction::new());
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        let mut client = Client::new(config, Box::new(StaleMock(mock.clone())), &Secrets::new());
        let err = execute(&mut client, "HassTurnOn", &json!({"name": "One"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert!(err.message.contains("may still have been performed"));
        assert_eq!(mock.call_count(), 1, "no second write after stale response");
    }

    #[test]
    fn structured_partial_or_warning_results_are_failures() {
        for body in [
            json!({"structuredContent": {"success": false, "error": "call failed"}}),
            json!({"structuredContent": {"success": true, "partial": true, "warnings": ["x"]}}),
            json!({"structuredContent": {"success": true, "errors": ["x"]}}),
            json!({"structuredContent": {"success": true, "warnings": ["slow"]}}),
        ] {
            let mut mock_impl = MockAction::new();
            mock_impl.fail_on = Some(1);
            mock_impl.fail_body = body;
            let mock = std::rc::Rc::new(mock_impl);
            let mut client = action_client(&mock);
            let err = execute(&mut client, "HassTurnOn", &json!({"name": "One"})).unwrap_err();
            assert!(matches!(err.kind, ErrorType::Intent), "{err}");
        }
    }

    #[test]
    fn named_group_target_is_rejected_before_any_service_call() {
        // Группа, помеченная ha_search is_group=true, не может быть целью
        // действия: ошибка вместо молчаливой фильтрации, 0 вызовов.
        let mock = std::rc::Rc::new(MockAction::with_groups());
        let mut client = action_client(&mock);
        let err = execute(&mut client, "HassTurnOn", &json!({"name": "Group Lights"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent), "{err}");
        assert!(err.message.contains("group or aggregate"), "{err}");
        // Внутренний ID не раскрывается.
        assert!(!err.message.contains("light.group_kitchen"));
        assert_eq!(mock.call_count(), 0);
    }

    #[test]
    fn mixed_bulk_containing_group_is_rejected_before_any_service_call() {
        // Bulk по area+domain, где среди совпавших есть группа: набор не
        // урезается молча — действие отменяется целиком, 0 вызовов.
        let mock = std::rc::Rc::new(MockAction::with_groups());
        let mut client = action_client(&mock);
        let err = execute(
            &mut client,
            "HassTurnOn",
            &json!({"area": "Kitchen", "domain": "light"}),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent), "{err}");
        assert!(err.message.contains("Group Lights"), "{err}");
        assert_eq!(mock.call_count(), 0);
    }

    #[test]
    fn group_domain_entity_is_rejected_outright() {
        let mock = std::rc::Rc::new(MockAction::with_groups());
        let mut client = action_client(&mock);
        let err = execute(&mut client, "HassTurnOn", &json!({"name": "All Lights"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent), "{err}");
        assert!(err.message.contains("domain 'group'"), "{err}");
        assert_eq!(mock.call_count(), 0);
    }

    #[test]
    fn non_boolean_group_flag_is_fail_closed() {
        // is_group не булево значение: нельзя считать гарантией листа —
        // цель отклоняется до вызова сервиса.
        let mock = std::rc::Rc::new(MockAction::with_groups());
        let mut client = action_client(&mock);
        let err = execute(&mut client, "HassTurnOn", &json!({"name": "Odd Flag"})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent), "{err}");
        assert!(err.message.contains("group or aggregate"), "{err}");
        assert_eq!(mock.call_count(), 0);
    }

    #[test]
    fn leaf_targets_are_unaffected_by_group_fixture_presence() {
        // Листья в том же каталоге с группами разрешаются и исполняются
        // как раньше; хаos-группа в bulk с точечным доменом не участвует.
        let mock = std::rc::Rc::new(MockAction::with_groups());
        let mut client = action_client(&mock);
        let result = execute(&mut client, "HassTurnOn", &json!({"name": "One"})).unwrap();
        assert_eq!(result["response_type"], json!("action_done"));
        assert_eq!(mock.call_count(), 1);
        assert_eq!(mock.calls()[0]["entity_id"], json!("light.a"));
        // Read-only HassGetState сохраняет прежнюю семантику: группу можно
        // прочитать, блокировка действует только на действия.
        let mock = std::rc::Rc::new(MockGetState::new(state_payload("on")));
        let mut client = get_state_client(&mock);
        let result = execute(&mut client, "HassGetState", &json!({"name": "One"})).unwrap();
        assert_eq!(result["data"]["states"][0]["state"], json!("on"));
    }
}
