use crate::client::Client;
use crate::discovery::{get_tool as find_tool, is_stale_tool_result, ToolDiscovery};
use crate::errors::{ErrorType, HaCliError};
use crate::models::Entity;
use crate::tool_result::{extract_text, success_failure_message, tool_error_message, truthy};
use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

/// Инструмент ha-mcp для отфильтрованного каталога сущностей
/// (docs/mcp_migration.md, этап 3, подпункт 1).
pub const HA_SEARCH_TOOL: &str = "ha_search";
/// Read-only обзор ha-mcp: источник списка доменов (`domain_stats`).
/// Полный каталог без фильтра `ha_search` не перечисляет (8.6.0 требует
/// query или domain/area/state filter).
pub const HA_OVERVIEW_TOOL: &str = "ha_get_overview";
/// Размер одной страницы `ha_search`; полный каталог собирается по страницам.
pub const HA_SEARCH_PAGE_LIMIT: i64 = 200;
/// Верхняя граница числа страниц: защита от зацикливания пагинации.
pub const HA_SEARCH_MAX_PAGES: usize = 1000;
/// Поля, которые запрашиваются у `ha_search` через `result_fields`:
/// без них ответ не содержит `area` и `aliases`, нужные модели `Entity`;
/// `is_group` — opt-in разметка агрегатов (групп с member entities):
/// действия обязаны видеть её, чтобы не расширить эффект за пределы
/// отфильтрованного набора листьев (этап 4.2). Если установка/версия
/// ha-mcp не размечает агрегаты, поле просто отсутствует — см.
/// `Entity.is_group` про обработку отсутствия.
pub const HA_SEARCH_RESULT_FIELDS: &[&str] = &[
    "entity_id",
    "friendly_name",
    "domain",
    "state",
    "area",
    "aliases",
    "is_group",
];

/// Сырой результат `context --raw`: агрегированный результат всех страниц
/// `ha_search` (`entities`, `entity_total_matches`, `partial`, `errors`,
/// `source`, `pages`).
pub fn get_raw_result(client: &mut Client) -> Result<Json, HaCliError> {
    collect_ha_search_entities(client)
}

/// Полный отфильтрованный каталог сущностей ha-mcp.
pub fn get_live_context(client: &mut Client) -> Result<Json, HaCliError> {
    collect_ha_search_entities(client)
}

/// Сборка полного каталога ha-mcp: список доменов приходит из read-only
/// `ha_get_overview(fields=["domain_stats"])`, сами сущности — ТОЛЬКО из
/// постраничных `ha_search(domain_filter=…)`. Неполный набор (`partial`,
/// `errors`, противоречивый `entity_next_offset`, несовпадение суммы
/// `entity_total_matches`) никогда не выдаётся как полный.
pub fn collect_ha_search_entities(client: &mut Client) -> Result<Json, HaCliError> {
    let context_error = |message: String| HaCliError::new(ErrorType::Context, message);
    let mut discovery = ToolDiscovery::for_endpoint(&client.config.mcp_url);
    let domains = collect_domains_via_overview(client, &mut discovery)?;
    let mut search_tool = discovery.get_tool(client, HA_SEARCH_TOOL)?;
    let mut entities: Vec<Json> = Vec::new();
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    let mut total: Option<i64> = None;
    let mut pages = 0usize;
    for domain in &domains {
        let mut offset = 0i64;
        loop {
            pages += 1;
            if pages > HA_SEARCH_MAX_PAGES {
                return Err(context_error(
                    "ha_search pagination exceeds the page limit".to_string(),
                ));
            }
            let arguments = json!({
                "domain_filter": domain,
                "limit": HA_SEARCH_PAGE_LIMIT,
                "offset": offset,
                "result_fields": HA_SEARCH_RESULT_FIELDS,
            });
            let mut result = client.tools_call(&search_tool.mcp_name, &arguments)?;
            if is_stale_tool_result(&result) {
                let mapping = discovery.refresh(client)?;
                search_tool = find_tool(&mapping, HA_SEARCH_TOOL)?.clone();
                result = client.tools_call(&search_tool.mcp_name, &arguments)?;
            }
            // Тот же разбор результата инструмента: isError, success:false,
            // structuredContent или JSON в тексте content.
            let page = parse_live_context(&result)?;
            if truthy(page.get("partial")) {
                return Err(context_error(format!(
                    "ha_search for domain '{domain}' returned a partial result; \
                     the entity catalog is incomplete"
                )));
            }
            let error_count = page
                .get("errors")
                .and_then(Json::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if error_count > 0 {
                // Содержимое errors не выводится: оно может отражать детали
                // установки; достаточно количества и домена.
                return Err(context_error(format!(
                    "ha_search for domain '{domain}' reported {error_count} error(s); \
                     the entity catalog is incomplete"
                )));
            }
            if let Some(items) = page.get("entities").and_then(Json::as_array) {
                for entity in items {
                    // Сущности добавляются только из отфильтрованных
                    // результатов ha_search; дубликаты ID пропускаются.
                    let id = entity.get("entity_id").and_then(Json::as_str);
                    match id {
                        Some(id) => {
                            if seen_ids.insert(id.to_string()) {
                                entities.push(entity.clone());
                            }
                        }
                        None => {
                            if seen_ids.insert(format!("\u{0}index:{}", entities.len())) {
                                entities.push(entity.clone());
                            }
                        }
                    }
                }
            }
            if offset == 0 {
                if let Some(value) = page.get("entity_total_matches").and_then(Json::as_i64) {
                    *total.get_or_insert(0) += value;
                }
            }
            if truthy(page.get("entity_has_more")) {
                match page.get("entity_next_offset").and_then(Json::as_i64) {
                    Some(next) if next > offset => offset = next,
                    // has_more=true без корректного next_offset: следующая
                    // страница недоступна — неполный каталог не выдаём.
                    _ => {
                        return Err(context_error(format!(
                            "ha_search for domain '{domain}' reported entity_has_more \
                             without a valid entity_next_offset"
                        )))
                    }
                }
            } else {
                break;
            }
        }
    }
    if let Some(total) = total {
        if entities.len() as i64 != total {
            return Err(context_error(format!(
                "ha_search returned {} of {total} matching entities; the entity catalog is incomplete",
                entities.len()
            )));
        }
    }
    Ok(json!({
        "entities": entities,
        "entity_total_matches": total,
        "entity_has_more": false,
        "entity_next_offset": Json::Null,
        "partial": false,
        "errors": [],
        "source": HA_SEARCH_TOOL,
        "pages": pages,
        "domains": domains.len(),
    }))
}

/// Список доменов для перебора: `ha_get_overview(fields=["domain_stats"])`.
/// Сами сущности из обзора не берутся — только имена доменов.
fn collect_domains_via_overview(
    client: &mut Client,
    discovery: &mut ToolDiscovery,
) -> Result<Vec<String>, HaCliError> {
    let context_error = |message: String| HaCliError::new(ErrorType::Context, message);
    let mut tool = discovery.get_tool(client, HA_OVERVIEW_TOOL)?;
    let arguments = json!({"fields": ["domain_stats"]});
    let mut result = client.tools_call(&tool.mcp_name, &arguments)?;
    if is_stale_tool_result(&result) {
        let mapping = discovery.refresh(client)?;
        tool = find_tool(&mapping, HA_OVERVIEW_TOOL)?.clone();
        result = client.tools_call(&tool.mcp_name, &arguments)?;
    }
    let overview = parse_live_context(&result)?;
    let stats = find_domain_stats(&overview).ok_or_else(|| {
        context_error(
            "ha_get_overview response has no domain_stats; the entity catalog is unavailable"
                .to_string(),
        )
    })?;
    let mut domains = parse_domain_list(stats);
    if domains.is_empty() {
        return Err(context_error(
            "ha_get_overview returned an empty domain_stats; the entity catalog is unavailable"
                .to_string(),
        ));
    }
    domains.sort();
    domains.dedup();
    Ok(domains)
}

/// `domain_stats` в корне ответа или в одном из вложенных объектов
/// верхнего уровня (например под ключом `overview`).
fn find_domain_stats(overview: &Json) -> Option<&Json> {
    if let Some(stats) = overview.get("domain_stats") {
        if !stats.is_null() {
            return Some(stats);
        }
    }
    overview
        .as_object()?
        .values()
        .find_map(|value| value.get("domain_stats").filter(|stats| !stats.is_null()))
}

/// Домены из `domain_stats`. Поддерживаются фактические варианты формы:
/// массив объектов с полем `domain` (опционально `count`) или объект
/// `{"domain": count}`.
fn parse_domain_list(stats: &Json) -> Vec<String> {
    match stats {
        Json::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("domain")
                    .and_then(Json::as_str)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string)
            })
            .collect(),
        Json::Object(map) => map.keys().filter(|d| !d.is_empty()).cloned().collect(),
        _ => Vec::new(),
    }
}

/// Перенос `parse_live_context`.
pub fn parse_live_context(result: &Json) -> Result<Json, HaCliError> {
    let context_error = |message: &str| HaCliError::new(ErrorType::Context, message.to_string());
    if !result.is_object() {
        return Err(context_error("unexpected tool result type"));
    }
    // Ошибки инструмента проверяются ДО требования content: структурированная
    // ToolError при `isError` без content — настоящая ошибка, а не «нет content».
    if truthy(result.get("isError")) {
        return Err(context_error(&tool_error_message(
            result,
            "context request failed",
        )));
    }
    let structured = match result.get("structuredContent") {
        Some(value) if !value.is_null() => value,
        _ => {
            if !result.get("content").is_some_and(Json::is_array) {
                return Err(context_error("tool result has no content"));
            }
            let text = extract_text(result);
            if text.is_empty() {
                return Err(context_error("tool result has no text content"));
            }
            let parsed: Json = serde_json::from_str(&text)
                .map_err(|_| context_error("tool result text is not valid JSON"))?;
            if !parsed.is_object() {
                return Err(context_error("tool result JSON is not an object"));
            }
            if let Some(message) =
                success_failure_message(&parsed, "context request reported failure")
            {
                return Err(context_error(&message));
            }
            return Ok(parsed);
        }
    };
    if let Some(message) = success_failure_message(structured, "context request reported failure") {
        return Err(context_error(&message));
    }
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

/// Перенос `build_context` (areas, полная сортировка).
pub fn build_context(live_context: &Json) -> Json {
    let mut areas: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for entity in normalize_entities(live_context) {
        let domains = areas.entry(entity.area.clone()).or_default();
        let names = domains.entry(entity.domain.clone()).or_default();
        if !names.contains(&entity.name) {
            names.push(entity.name);
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
