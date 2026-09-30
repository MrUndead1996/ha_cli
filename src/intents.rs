use crate::client::Client;
use crate::context;
use crate::discovery::{get_tool as find_tool, is_stale_tool_result, ToolDiscovery};
use crate::errors::{ErrorType, HaCliError};
use crate::security;
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
    if !result.get("content").is_some_and(Json::is_array) {
        return Err(intent_error("tool result has no content"));
    }
    if truthy(result.get("isError")) {
        let text = extract_text(result);
        let message = if text.is_empty() {
            "intent execution failed"
        } else {
            &text
        };
        return Err(intent_error(message));
    }
    let text = extract_text(result);
    let intent_response = parse_intent_response(&text);
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

/// Перенос `_extract_text`.
fn extract_text(result: &Json) -> String {
    let Some(content) = result.get("content").and_then(Json::as_array) else {
        return String::new();
    };
    for item in content {
        if item.get("type").and_then(Json::as_str) == Some("text") {
            return match item.get("text") {
                Some(Json::String(text)) => text.clone(),
                _ => String::new(),
            };
        }
    }
    String::new()
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

/// Аналог Python-проверки истинности для `result.get("isError")`.
fn truthy(value: Option<&Json>) -> bool {
    match value {
        None | Some(Json::Null) => false,
        Some(Json::Bool(flag)) => *flag,
        Some(Json::Number(number)) => number.as_f64().is_none_or(|f| f != 0.0),
        Some(Json::String(text)) => !text.is_empty(),
        Some(Json::Array(items)) => !items.is_empty(),
        Some(Json::Object(object)) => !object.is_empty(),
    }
}
