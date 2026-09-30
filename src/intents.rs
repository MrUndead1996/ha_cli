use crate::client::Client;
use crate::context;
use crate::discovery::{get_tool as find_tool, is_stale_tool_result, ToolDiscovery};
use crate::errors::{ErrorType, HaCliError};
use crate::resolver;
use crate::security;
use crate::tool_result::{extract_text, success_failure_message, tool_error_message, truthy};
use serde_json::{json, Map, Value as Json};

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
    // Этап 3.2 (docs/mcp_migration.md): на ha-mcp (`mcp_url` задан) действие
    // сначала строго разрешается по отфильтрованному каталогу `ha_search` —
    // ноль/неоднозначные совпадения и превышение bound массового набора
    // отклоняются ДО любого вызова инструмента действия. Путь исполнения
    // (ha_call_service) появится на этапе 4; HassGetState мигрирует на 3.3.
    // Assist endpoint без `mcp_url` работает как раньше.
    if client.config.mcp_url.is_some() && intent != "HassGetState" {
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
}
