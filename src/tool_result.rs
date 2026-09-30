use serde_json::Value as Json;

/// Первый content-элемент типа "text" (пустая строка, если текста нет).
pub fn extract_text(result: &Json) -> String {
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

/// Аналог Python-проверки истинности для `result.get("isError")`.
pub fn truthy(value: Option<&Json>) -> bool {
    match value {
        None | Some(Json::Null) => false,
        Some(Json::Bool(flag)) => *flag,
        Some(Json::Number(number)) => number.as_f64().is_none_or(|f| f != 0.0),
        Some(Json::String(text)) => !text.is_empty(),
        Some(Json::Array(items)) => !items.is_empty(),
        Some(Json::Object(object)) => !object.is_empty(),
    }
}

/// Структурированное сообщение ToolError: `error` или `message` в объекте
/// (`structuredContent` или разобранный текст результата).
pub fn structured_error_message(value: Option<&Json>) -> Option<String> {
    let object = value?.as_object()?;
    for key in ["error", "message"] {
        if let Some(Json::String(text)) = object.get(key) {
            if !text.is_empty() {
                return Some(text.clone());
            }
        }
    }
    None
}

/// Сообщение об ошибке инструмента при `isError=true`: текст content имеет
/// приоритет; иначе структурированное ToolError-сообщение; иначе fallback.
pub fn tool_error_message(result: &Json, fallback: &str) -> String {
    let text = extract_text(result);
    if !text.is_empty() {
        return text;
    }
    structured_error_message(result.get("structuredContent"))
        .unwrap_or_else(|| fallback.to_string())
}

/// `{ "success": false, ... }` в объекте — сообщение об ошибке (деталь из
/// `error`/`message` или fallback); `success: true` или отсутствие ключа —
/// успешный/необёрнутый результат.
pub fn success_failure_message(value: &Json, fallback: &str) -> Option<String> {
    let success = value.as_object()?.get("success")?;
    if matches!(success, Json::Bool(true)) {
        return None;
    }
    Some(structured_error_message(Some(value)).unwrap_or_else(|| fallback.to_string()))
}
