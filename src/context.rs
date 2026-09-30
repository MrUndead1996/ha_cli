use crate::client::Client;
use crate::discovery::{get_tool as find_tool, is_stale_tool_result, ToolDiscovery};
use crate::errors::{ErrorType, HaCliError};
use crate::models::Entity;
use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

pub const ASSIST_CONTEXT_TOOL: &str = "GetLiveContext";

/// Перенос `get_raw_result`.
pub fn get_raw_result(client: &mut Client) -> Result<Json, HaCliError> {
    let mut discovery = ToolDiscovery::new(None);
    let tool = discovery.get_tool(client, ASSIST_CONTEXT_TOOL)?;
    let arguments = build_arguments(&tool.input_schema)?;
    let result = client.tools_call(&tool.mcp_name, &arguments)?;
    if is_stale_tool_result(&result) {
        let mapping = discovery.refresh(client)?;
        let tool = find_tool(&mapping, ASSIST_CONTEXT_TOOL)?;
        let arguments = build_arguments(&tool.input_schema)?;
        return client.tools_call(&tool.mcp_name, &arguments);
    }
    Ok(result)
}

/// Перенос `get_live_context`.
pub fn get_live_context(client: &mut Client) -> Result<Json, HaCliError> {
    let raw = get_raw_result(client)?;
    parse_live_context(&raw)
}

/// Перенос `build_arguments`: defaults из inputSchema, required без
/// default -> ContextError.
pub fn build_arguments(input_schema: &Json) -> Result<Json, HaCliError> {
    let mut arguments = Map::new();
    let Some(properties) = input_schema.get("properties").and_then(Json::as_object) else {
        return Ok(Json::Object(arguments));
    };
    let required: Vec<&str> = input_schema
        .get("required")
        .and_then(Json::as_array)
        .map(|items| items.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    for (key, spec) in properties {
        let default = spec.get("default").filter(|value| !value.is_null());
        match default {
            Some(value) => {
                arguments.insert(key.clone(), value.clone());
            }
            None => {
                if required.contains(&key.as_str()) {
                    return Err(HaCliError::new(
                        ErrorType::Context,
                        format!(
                            "{ASSIST_CONTEXT_TOOL} requires argument '{key}' with no default value"
                        ),
                    ));
                }
            }
        }
    }
    Ok(Json::Object(arguments))
}

/// Перенос `parse_live_context`.
pub fn parse_live_context(result: &Json) -> Result<Json, HaCliError> {
    let context_error = |message: &str| HaCliError::new(ErrorType::Context, message.to_string());
    if !result.is_object() {
        return Err(context_error("unexpected tool result type"));
    }
    if !result.get("content").is_some_and(Json::is_array) {
        return Err(context_error("tool result has no content"));
    }
    if truthy(result.get("isError")) {
        let text = extract_text(result);
        let message = if text.is_empty() {
            "context request failed"
        } else {
            &text
        };
        return Err(context_error(message));
    }
    let structured = match result.get("structuredContent") {
        Some(value) if !value.is_null() => value,
        _ => {
            let text = extract_text(result);
            if text.is_empty() {
                return Err(context_error("tool result has no text content"));
            }
            let parsed: Json = serde_json::from_str(&text)
                .map_err(|_| context_error("tool result text is not valid JSON"))?;
            if !parsed.is_object() {
                return Err(context_error("tool result JSON is not an object"));
            }
            return unwrap_live_context(&parsed);
        }
    };
    if !structured.is_object() {
        return Err(context_error("structuredContent is not a JSON object"));
    }
    Ok(structured.clone())
}

/// Перенос `normalize_entities`.
pub fn normalize_entities(live_context: &Json) -> Vec<Entity> {
    let entities = live_context.get("entities").and_then(Json::as_array);
    let Some(entities) = entities else {
        return Vec::new();
    };
    entities.iter().filter_map(Entity::from_raw).collect()
}

/// Перенос `query_state` (fallback HassGetState через live context).
pub fn query_state(live_context: &Json, target: &Json) -> Result<Json, HaCliError> {
    let invalid_arguments =
        |message: &str| HaCliError::new(ErrorType::InvalidArguments, message.to_string());
    let target_object = target.as_object();
    let string_selector = |key: &str| -> Option<&str> {
        target_object
            .and_then(|object| object.get(key))
            .and_then(Json::as_str)
            .filter(|value| !value.is_empty())
    };
    let selectors = Selectors {
        area: string_selector("area"),
        domain: string_selector("domain"),
        name: string_selector("name"),
    };
    if selectors.area.is_none() && selectors.domain.is_none() && selectors.name.is_none() {
        return Err(invalid_arguments(
            "HassGetState requires semantic area, domain or name",
        ));
    }
    let matches: Vec<Entity> = normalize_entities(live_context)
        .into_iter()
        .filter(|entity| matches_target(entity, &selectors))
        .collect();
    if matches.is_empty() {
        return Err(HaCliError::new(
            ErrorType::Intent,
            "no exposed entity matches the semantic target".to_string(),
        ));
    }
    let mut states = Vec::new();
    let mut speech_parts = Vec::new();
    for entity in &matches {
        // Порядок ключей важен для паритета вывода с Python-версией.
        let mut item = Map::new();
        item.insert("area".to_string(), Json::String(entity.area.clone()));
        item.insert("domain".to_string(), Json::String(entity.domain.clone()));
        item.insert("name".to_string(), Json::String(entity.name.clone()));
        item.insert(
            "state".to_string(),
            match &entity.state {
                Some(state) => Json::String(state.clone()),
                None => Json::Null,
            },
        );
        let state_text = entity
            .state
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        speech_parts.push(format!("{}: {}", entity.name, state_text));
        states.push(Json::Object(item));
    }
    Ok(json!({
        "ok": true,
        "response_type": "query_answer",
        "speech": speech_parts.join("; "),
        "data": {"states": states},
    }))
}

/// Перенос `build_context` (areas + aliases, полная сортировка).
pub fn build_context(live_context: &Json) -> Json {
    let mut areas: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut area_aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entity in normalize_entities(live_context) {
        let domains = areas.entry(entity.area.clone()).or_default();
        let names = domains.entry(entity.domain.clone()).or_default();
        if !names.contains(&entity.name) {
            names.push(entity.name);
        }
        for alias in &entity.aliases {
            if *alias == entity.area {
                continue;
            }
            area_aliases
                .entry(entity.area.clone())
                .or_default()
                .insert(alias.clone());
        }
    }
    // Финальный вывод полностью отсортирован (аналог _sort_areas/_sort_aliases).
    let mut areas_json = Map::new();
    for (area, domains) in areas {
        let mut domains_json = Map::new();
        for (domain, mut names) in domains {
            names.sort();
            names.dedup();
            domains_json.insert(
                domain,
                Json::Array(names.into_iter().map(Json::String).collect()),
            );
        }
        areas_json.insert(area, Json::Object(domains_json));
    }
    let mut context = Map::new();
    context.insert("areas".to_string(), Json::Object(areas_json));
    if !area_aliases.is_empty() {
        let mut aliases_json = Map::new();
        for (area, aliases) in area_aliases {
            aliases_json.insert(
                area,
                Json::Array(aliases.into_iter().map(Json::String).collect()),
            );
        }
        context.insert("area_aliases".to_string(), Json::Object(aliases_json));
    }
    Json::Object(context)
}

/// Перенос `compact_context`: только плоский объект areas.
pub fn compact_context(context: &Json) -> Json {
    context
        .get("areas")
        .and_then(Json::as_object)
        .map(|areas| Json::Object(areas.clone()))
        .unwrap_or_else(|| Json::Object(Map::new()))
}

/// Перенос `serialized_size`: длина компактной JSON-сериализации
/// (ensure_ascii=False, separators=(",", ":")). Python len() считает
/// символы, а не байты — важно для не-ASCII имён.
pub fn serialized_size(data: &Json) -> usize {
    serde_json::to_string(data)
        .map(|text| text.chars().count())
        .unwrap_or(0)
}

/// Перенос `size_report`.
pub fn size_report(context: &Json) -> Json {
    let full = serialized_size(&with_ok(context));
    let compact = serialized_size(&with_ok(&compact_context(context)));
    json!({"full": full, "compact": compact})
}

fn with_ok(data: &Json) -> Json {
    let mut merged = Map::new();
    merged.insert("ok".to_string(), Json::Bool(true));
    if let Some(object) = data.as_object() {
        for (key, value) in object {
            merged.insert(key.clone(), value.clone());
        }
    }
    Json::Object(merged)
}

/// Перенос `_unwrap_live_context`.
fn unwrap_live_context(parsed: &Json) -> Result<Json, HaCliError> {
    let context_error = |message: &str| HaCliError::new(ErrorType::Context, message.to_string());
    let (Some(success), Some(result)) = (parsed.get("success"), parsed.get("result")) else {
        return Ok(parsed.clone());
    };
    if !matches!(success, Json::Bool(true)) {
        return Err(context_error("GetLiveContext reported failure"));
    }
    let Some(text) = result.as_str() else {
        return Err(context_error("GetLiveContext result is not text"));
    };
    let entities = parse_context_entities(text)?;
    Ok(json!({"entities": entities}))
}

/// Перенос `_parse_context_entities` — парсер текстового формата
/// "Live Context:" (дословно, включая логику отступов).
fn parse_context_entities(text: &str) -> Result<Vec<Json>, HaCliError> {
    let context_error = |message: &str| HaCliError::new(ErrorType::Context, message.to_string());
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() || !lines[0].trim().starts_with("Live Context:") {
        return Err(context_error("GetLiveContext result has an unknown format"));
    }
    let first_entity = lines[0]
        .trim()
        .strip_prefix("Live Context:")
        .unwrap_or("")
        .trim();
    if first_entity.starts_with("- names:") {
        lines[0] = first_entity;
    } else {
        lines.remove(0);
    }
    let mut entities: Vec<Json> = Vec::new();
    let mut current: Option<Map<String, Json>> = None;
    for line in lines {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        if line.starts_with("- names:") {
            if let Some(previous) = current.take() {
                entities.push(Json::Object(previous));
            }
            let mut entity = Map::new();
            entity.insert("name".to_string(), Json::String(field_value(line)));
            entity.insert("area".to_string(), Json::Null);
            entity.insert("domain".to_string(), Json::Null);
            entity.insert("state".to_string(), Json::Null);
            entity.insert("capabilities".to_string(), Json::Object(Map::new()));
            current = Some(entity);
            continue;
        }
        if current.is_none() || !line.starts_with("  ") {
            return Err(context_error("GetLiveContext entity has an unknown format"));
        }
        let current = current.as_mut().unwrap();
        if line.starts_with("    ") {
            let (key, value) = split_field(stripped)?;
            let capabilities = current
                .entry("capabilities".to_string())
                .or_insert_with(|| Json::Object(Map::new()));
            if let Some(object) = capabilities.as_object_mut() {
                object.insert(key, Json::String(value));
            }
            continue;
        }
        let (key, value) = split_field(stripped)?;
        match key.as_str() {
            "areas" => {
                current.insert("area".to_string(), Json::String(value));
            }
            "domain" | "state" => {
                current.insert(key, Json::String(value));
            }
            "attributes" => {}
            _ => {
                let capabilities = current
                    .entry("capabilities".to_string())
                    .or_insert_with(|| Json::Object(Map::new()));
                if let Some(object) = capabilities.as_object_mut() {
                    object.insert(key, Json::String(value));
                }
            }
        }
    }
    if let Some(previous) = current.take() {
        entities.push(Json::Object(previous));
    }
    Ok(entities)
}

/// Перенос `_field_value`.
fn field_value(line: &str) -> String {
    let stripped = line.trim().strip_prefix("- ").unwrap_or(line.trim());
    split_field(stripped)
        .map(|(_, value)| value)
        .unwrap_or_default()
}

/// Перенос `_split_field`: разделение по первому ':', обрезка значения
/// и снятие парных кавычек.
fn split_field(line: &str) -> Result<(String, String), HaCliError> {
    let context_error = |message: &str| HaCliError::new(ErrorType::Context, message.to_string());
    let (key, value) = line
        .split_once(':')
        .ok_or_else(|| context_error("GetLiveContext field has an unknown format"))?;
    let mut value = value.trim();
    let bytes = value.as_bytes();
    if value.len() >= 2
        && bytes[0] == bytes[value.len() - 1]
        && (bytes[0] == b'"' || bytes[0] == b'\'')
    {
        value = &value[1..value.len() - 1];
    }
    Ok((key.to_string(), value.to_string()))
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

/// Семантические селекторы target (аналог dict selectors в Python).
struct Selectors<'a> {
    area: Option<&'a str>,
    domain: Option<&'a str>,
    name: Option<&'a str>,
}

/// Перенос `_matches_target`.
fn matches_target(entity: &Entity, selectors: &Selectors<'_>) -> bool {
    if let Some(area) = selectors.area {
        let mut candidates = vec![entity.area.as_str()];
        candidates.extend(entity.aliases.iter().map(String::as_str));
        if !equals_any(area, &candidates) {
            return false;
        }
    }
    if let Some(domain) = selectors.domain {
        if !equals_any(domain, &[entity.domain.as_str()]) {
            return false;
        }
    }
    if let Some(name) = selectors.name {
        if !equals_any(name, &[entity.name.as_str()]) {
            return false;
        }
    }
    true
}

/// Перенос `_equals_any`. Python использует `str.casefold()`;
/// для ASCII-имён областей достаточно `to_lowercase()` (решение
/// зафиксировано в docs/migration.md).
fn equals_any(value: &str, candidates: &[&str]) -> bool {
    let expected = value.trim().to_lowercase();
    candidates
        .iter()
        .any(|candidate| candidate.trim().to_lowercase() == expected)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_field_strips_matching_quotes() {
        assert_eq!(
            split_field("state: 'off'").unwrap(),
            ("state".into(), "off".into())
        );
        assert_eq!(
            split_field("state: \"off\"").unwrap(),
            ("state".into(), "off".into())
        );
        assert_eq!(
            split_field("state: off").unwrap(),
            ("state".into(), "off".into())
        );
        assert!(split_field("broken").is_err());
    }
}
