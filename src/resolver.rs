use crate::client::Client;
use crate::context;
use crate::errors::{ErrorType, HaCliError};
use crate::models::Entity;
use serde_json::Value as Json;
use std::collections::BTreeSet;

/// Верхняя граница числа целей массового действия. Набор разрешается
/// полностью ДО выполнения; превышение границы — ошибка, а не усечение
/// набора (docs/mcp_migration.md, этап 3, подпункт 2).
pub const MAX_ACTION_TARGETS: usize = 64;

/// Семантические селекторы действия: `area` / `domain` / `name`.
/// Прямой `entity_id` сюда не попадает: он отклоняется
/// `security::validate_entity_payload` до сети.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    pub area: Option<String>,
    pub domain: Option<String>,
    pub name: Option<String>,
}

impl Target {
    pub fn is_empty(&self) -> bool {
        self.area.is_none() && self.domain.is_none() && self.name.is_none()
    }
}

/// Результат разрешения: точечная сущность (по `name`, ровно одно
/// совпадение) либо массовое действие по явно указанной area/domain.
#[derive(Debug, Clone)]
pub enum Resolution {
    Named(Entity),
    Bulk {
        area: Option<String>,
        domain: Option<String>,
        entities: Vec<Entity>,
    },
}

/// Полностью разрешённая цель: набор зафиксирован (bound), готов к
/// выполнению этапа 4 (ha_call_service / ha_get_state).
#[derive(Debug, Clone)]
pub struct Prepared {
    pub target: Target,
    pub resolution: Resolution,
    /// Зафиксированное число целей: ровно `entities.len()`.
    pub bound: usize,
}

/// Чтение селекторов из пользовательского payload. Значения обрезаются;
/// пустые строки считаются отсутствующими. Payload без единого селектора —
/// ошибка (произвольный вызов без цели не разрешён).
pub fn extract_target(payload: &Json) -> Result<Target, HaCliError> {
    let object = payload.as_object();
    let selector = |key: &str| -> Option<String> {
        object
            .and_then(|object| object.get(key))
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let target = Target {
        area: selector("area"),
        domain: selector("domain"),
        name: selector("name"),
    };
    if target.is_empty() {
        return Err(HaCliError::new(
            ErrorType::InvalidArguments,
            "action requires semantic area, domain or name",
        ));
    }
    Ok(target)
}

/// Точное сравнение без учёта регистра, как в `context::equals_any`
/// (trim + lowercase; casefold не используется — решение зафиксировано
/// в docs/migration.md). Никакого нечёткого сопоставления.
fn eq_ci(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Дедупликация по внутреннему `entity_id`: повтор записи каталога — та же
/// цель, а не вторая; она не должна ни дублировать массовый набор, ни
/// делать точечное имя «неоднозначным». Записи без `entity_id` (Assist)
/// не дедуплицируются.
pub fn dedupe_entities(entities: Vec<Entity>) -> Vec<Entity> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    entities
        .into_iter()
        .filter(|entity| match &entity.entity_id {
            Some(id) => seen.insert(id.clone()),
            None => true,
        })
        .collect()
}

/// Сопоставление сущности селекторам. `area` сопоставляется только области
/// и её алиасам (`Entity.aliases`, для ha-mcp пусты); алиасы СУЩНОСТИ
/// (`entity_aliases` из `ha_search.aliases`) областью НЕ считаются и
/// используются только как дополнительные точные имена для `name`.
fn matches(entity: &Entity, target: &Target) -> bool {
    if let Some(area) = &target.area {
        let mut candidates = vec![entity.area.as_str()];
        candidates.extend(entity.aliases.iter().map(String::as_str));
        if !candidates.iter().any(|candidate| eq_ci(candidate, area)) {
            return false;
        }
    }
    if let Some(domain) = &target.domain {
        if !eq_ci(&entity.domain, domain) {
            return false;
        }
    }
    if let Some(name) = &target.name {
        let mut candidates = vec![entity.name.as_str()];
        candidates.extend(entity.entity_aliases.iter().map(String::as_str));
        if !candidates.iter().any(|candidate| eq_ci(candidate, name)) {
            return false;
        }
    }
    true
}

/// Строгое разрешение цели по каталогу (результат `get_live_context`):
/// - точечный `name`: ноль совпадений или больше одного — ошибки, первый
///   «похожий» никогда не выбирается;
/// - массовое действие — только по явно указанной area/domain (или обеим);
///   набор разрешается полностью до выполнения.
///
/// Каждая совпавшая цель обязана иметь валидный непустой внутренний
/// `entity_id` из результатов `ha_search`: запись каталога без него
/// (malformed) — fail-closed ошибка до любого вызова инструмента,
/// а не цель без идентификатора.
pub fn resolve(catalog: &Json, target: &Target) -> Result<Prepared, HaCliError> {
    let entities = dedupe_entities(context::normalize_entities(catalog));
    let mut matched: Vec<Entity> = Vec::new();
    for entity in entities {
        if !matches(&entity, target) {
            continue;
        }
        let valid_id = entity
            .entity_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty());
        if !valid_id {
            return Err(HaCliError::new(
                ErrorType::Intent,
                "entity catalog returned a matched entity without a valid internal ID; \
                 the target set is not resolvable",
            ));
        }
        matched.push(entity);
    }
    let matches = matched;
    if let Some(name) = &target.name {
        return match matches.len() {
            0 => Err(HaCliError::new(
                ErrorType::Intent,
                format!("no exposed entity matches name '{name}'"),
            )),
            1 => Ok(Prepared {
                target: target.clone(),
                bound: 1,
                resolution: Resolution::Named(matches.into_iter().next().unwrap()),
            }),
            count => Err(HaCliError::new(
                ErrorType::AmbiguousTool,
                format!(
                    "ambiguous name '{name}': {count} exposed entities match; \
                     no entity was selected"
                ),
            )),
        };
    }
    if matches.is_empty() {
        return Err(HaCliError::new(
            ErrorType::Intent,
            "no exposed entity matches the semantic target",
        ));
    }
    if matches.len() > MAX_ACTION_TARGETS {
        return Err(HaCliError::new(
            ErrorType::Intent,
            format!(
                "{} entities match the target, exceeding the action bound of \
                 {MAX_ACTION_TARGETS}; narrow area/domain",
                matches.len()
            ),
        ));
    }
    Ok(Prepared {
        target: target.clone(),
        bound: matches.len(),
        resolution: Resolution::Bulk {
            area: target.area.clone(),
            domain: target.domain.clone(),
            entities: matches,
        },
    })
}

/// Подготовка ha-mcp действия: рекурсивный запрет `entity_id` в payload
/// проверяется здесь НА ВХОДЕ (до любой сети), независимо от вызывающей
/// стороны — последующие этапы 3.3/4 не могут обойти его, использовав
/// `prepare_action` напрямую. Каталог собирается заново при каждом вызове
/// (`get_live_context` → `ha_search`); между действиями он НЕ кэшируется —
/// кэш схем инструментов не является кэшем доступных сущностей. Ошибки
/// разрешения возникают здесь, ДО вызова любого инструмента действия.
pub fn prepare_action(client: &mut Client, payload: &Json) -> Result<Prepared, HaCliError> {
    if let Err(message) = crate::security::validate_entity_payload(payload) {
        return Err(HaCliError::new(ErrorType::InvalidArguments, message));
    }
    let target = extract_target(payload)?;
    let catalog = context::get_live_context(client)?;
    resolve(&catalog, &target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{HttpResponse, Transport};
    use crate::config::{Config, McpAuth};
    use crate::security::Secrets;
    use serde_json::json;
    use std::cell::RefCell;

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

    fn catalog(entities: Vec<Json>) -> Json {
        json!({"entities": entities})
    }

    fn target_of(payload: &Json) -> Target {
        extract_target(payload).unwrap()
    }

    fn resolved_ids(prepared: &Prepared) -> Vec<String> {
        match &prepared.resolution {
            Resolution::Named(entity) => vec![entity.entity_id.clone().unwrap()],
            Resolution::Bulk { entities, .. } => entities
                .iter()
                .map(|entity| entity.entity_id.clone().unwrap())
                .collect(),
        }
    }

    #[test]
    fn target_requires_at_least_one_selector() {
        let err = extract_target(&json!({"brightness": 10})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::InvalidArguments));
        let err = extract_target(&json!({"area": "   "})).unwrap_err();
        assert!(matches!(err.kind, ErrorType::InvalidArguments));
    }

    #[test]
    fn exact_point_name_resolves_single_entity_case_insensitively() {
        let prepared = resolve(
            &catalog(vec![entity_raw(
                "light.kitchen",
                "Kitchen",
                "Ceiling Light",
            )]),
            &target_of(&json!({"name": "  ceiling LIGHT "})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 1);
        assert_eq!(resolved_ids(&prepared), vec!["light.kitchen"]);
    }

    #[test]
    fn point_name_matches_entity_alias_but_alias_is_not_an_area() {
        let mut raw = entity_raw("light.kitchen", "Kitchen", "Ceiling Light");
        raw["aliases"] = json!(["main lamp"]);
        // Алиас сущности работает как имя.
        let prepared = resolve(
            &catalog(vec![raw.clone()]),
            &target_of(&json!({"name": "main lamp"})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 1);
        // Алиас сущности НЕ является областью.
        let err = resolve(
            &catalog(vec![raw]),
            &target_of(&json!({"area": "main lamp"})),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
    }

    #[test]
    fn zero_point_name_matches_is_an_error() {
        let err = resolve(
            &catalog(vec![entity_raw(
                "light.kitchen",
                "Kitchen",
                "Ceiling Light",
            )]),
            &target_of(&json!({"name": "Missing Lamp"})),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
    }

    #[test]
    fn ambiguous_point_name_is_an_error_and_never_picks_first() {
        let err = resolve(
            &catalog(vec![
                entity_raw("light.a", "Kitchen", "Lamp"),
                entity_raw("light.b", "Office", "Lamp"),
            ]),
            &target_of(&json!({"name": "lamp"})),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::AmbiguousTool));
        assert!(err.message.contains('2'));
    }

    #[test]
    fn duplicates_of_one_entity_do_not_make_the_name_ambiguous() {
        let raw = entity_raw("light.kitchen", "Kitchen", "Ceiling Light");
        let prepared = resolve(
            &catalog(vec![raw.clone(), raw]),
            &target_of(&json!({"name": "ceiling light"})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 1);
    }

    #[test]
    fn bulk_by_area_resolves_the_whole_set() {
        let prepared = resolve(
            &catalog(vec![
                entity_raw("light.a", "Kitchen", "One"),
                entity_raw("switch.b", "Kitchen", "Two"),
                entity_raw("light.c", "Office", "Three"),
            ]),
            &target_of(&json!({"area": "kitchen"})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 2);
        assert_eq!(resolved_ids(&prepared), vec!["light.a", "switch.b"]);
    }

    #[test]
    fn bulk_by_domain_resolves_the_whole_set() {
        let prepared = resolve(
            &catalog(vec![
                entity_raw("light.a", "Kitchen", "One"),
                entity_raw("light.b", "Office", "Two"),
                entity_raw("switch.c", "Office", "Three"),
            ]),
            &target_of(&json!({"domain": "Light"})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 2);
        assert_eq!(resolved_ids(&prepared), vec!["light.a", "light.b"]);
    }

    #[test]
    fn bulk_by_area_and_domain_intersects_both() {
        let prepared = resolve(
            &catalog(vec![
                entity_raw("light.a", "Kitchen", "One"),
                entity_raw("light.b", "Office", "Two"),
                entity_raw("switch.c", "Kitchen", "Three"),
            ]),
            &target_of(&json!({"area": "Kitchen", "domain": "light"})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 1);
        assert_eq!(resolved_ids(&prepared), vec!["light.a"]);
    }

    #[test]
    fn unknown_area_or_domain_is_zero_matches_error() {
        let catalog = catalog(vec![entity_raw("light.a", "Kitchen", "One")]);
        let err = resolve(&catalog, &target_of(&json!({"area": "Attic"}))).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        let err = resolve(&catalog, &target_of(&json!({"domain": "fan"}))).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
    }

    #[test]
    fn hidden_entity_absent_from_catalog_is_not_resolvable() {
        // ha_search не возвращает скрытые сущности; резолвер видит только
        // каталог и обязан ответить «ноль совпадений».
        let catalog = catalog(vec![entity_raw("light.a", "Kitchen", "One")]);
        let err = resolve(&catalog, &target_of(&json!({"name": "Hidden Lamp"}))).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
    }

    #[test]
    fn bulk_set_above_bound_is_rejected_before_execution() {
        let entities: Vec<Json> = (0..MAX_ACTION_TARGETS + 1)
            .map(|index| entity_raw(&format!("switch.{index}"), "Garage", "Switch"))
            .collect();
        let err = resolve(&catalog(entities), &target_of(&json!({"area": "garage"}))).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Intent));
        assert!(err.message.contains(&MAX_ACTION_TARGETS.to_string()));
    }

    #[test]
    fn fuzzy_or_prefix_names_do_not_match() {
        let catalog = catalog(vec![entity_raw(
            "light.kitchen",
            "Kitchen",
            "Ceiling Light",
        )]);
        for guess in ["ceiling", "Ceiling Li", "light.ceiling"] {
            let err = resolve(&catalog, &target_of(&json!({"name": guess}))).unwrap_err();
            assert!(matches!(err.kind, ErrorType::Intent));
        }
    }

    #[test]
    fn matched_entity_without_valid_internal_id_is_fail_closed() {
        // Записи каталога без entity_id (или с пустым) не превращаются в
        // цель: ошибка ДО любого вызова инструмента.
        let mut missing = entity_raw("light.a", "Kitchen", "Ceiling Light");
        missing.as_object_mut().unwrap().remove("entity_id");
        let mut blank = entity_raw("light.a", "Kitchen", "Ceiling Light");
        blank
            .as_object_mut()
            .unwrap()
            .insert("entity_id".to_string(), json!("   "));
        for raw in [missing, blank] {
            let err = resolve(
                &catalog(vec![raw.clone()]),
                &target_of(&json!({"name": "ceiling light"})),
            )
            .unwrap_err();
            assert!(matches!(err.kind, ErrorType::Intent));
            // Массовый набор так же fail-closed.
            let err =
                resolve(&catalog(vec![raw]), &target_of(&json!({"area": "kitchen"}))).unwrap_err();
            assert!(matches!(err.kind, ErrorType::Intent));
        }
    }

    #[test]
    fn malformed_id_in_unmatched_entity_does_not_block_resolvable_targets() {
        // Не совпавшая запись без ID не мешает точечному разрешению другой.
        let mut bad = entity_raw("switch.bad", "Kitchen", "Broken");
        bad.as_object_mut().unwrap().remove("entity_id");
        let prepared = resolve(
            &catalog(vec![bad, entity_raw("light.a", "Kitchen", "Ceiling Light")]),
            &target_of(&json!({"name": "ceiling light"})),
        )
        .unwrap();
        assert_eq!(prepared.bound, 1);
        assert_eq!(resolved_ids(&prepared), vec!["light.a"]);
    }

    #[test]
    fn prepare_action_rejects_entity_id_alongside_selector_without_network() {
        // prepare_action сама валидирует payload: entity_id рядом с
        // корректным селектором отклоняется до любой сети (NoNetwork-путь:
        // счётчик ha_search обязан остаться нулём).
        let mock = std::rc::Rc::new(MockHaMcp::default());
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        let mut client = Client::new(config, Box::new(SharedMock(mock.clone())), &Secrets::new());
        for payload in [
            json!({"area": "Kitchen", "entity_id": "light.a"}),
            json!({"domain": "light", "data": {"entity_ids": ["light.a"]}}),
        ] {
            let err = prepare_action(&mut client, &payload).unwrap_err();
            assert!(matches!(err.kind, ErrorType::InvalidArguments));
        }
        assert_eq!(mock.search_count(), 0);
    }

    /// Скриптованный ha-mcp сервер: отвечает по имени вызванного
    /// инструмента, считая вызовы ha_search — для проверки отсутствия
    /// повторного использования каталога между действиями. Счётчики
    /// interior-mutable, поэтому один экземпляр доступен и транспорту
    /// клиента, и тесту.
    #[derive(Default)]
    struct MockHaMcp {
        search_calls: RefCell<usize>,
    }

    fn reply(id: u64, result: Json) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        }
    }

    impl MockHaMcp {
        fn search_count(&self) -> usize {
            *self.search_calls.borrow()
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
                                        entity_raw("light.a", "Kitchen", "One"),
                                        entity_raw("light.b", "Office", "Two"),
                                    ],
                                    "entity_total_matches": 2,
                                    "partial": false,
                                    "errors": [],
                                    "entity_has_more": false,
                                    "entity_next_offset": null,
                                }}),
                            ))
                        }
                        other => panic!("unexpected tool call: {other}"),
                    }
                }
                other => panic!("unexpected method: {other:?}"),
            }
        }
    }

    /// Обёртка: Client владеет Box<dyn Transport>, а тесту нужен доступ
    /// к счётчикам того же экземпляра.
    struct SharedMock(std::rc::Rc<MockHaMcp>);

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

    #[test]
    fn prepare_action_resolves_from_fresh_catalog_each_time() {
        let mock = std::rc::Rc::new(MockHaMcp::default());
        let config = Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: Some("http://ha.local/api/webhook/test".to_string()),
            mcp_auth: McpAuth::None,
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        };
        let mut client = Client::new(config, Box::new(SharedMock(mock.clone())), &Secrets::new());
        let payload = json!({"area": "Kitchen"});
        let first = prepare_action(&mut client, &payload).unwrap();
        let second = prepare_action(&mut client, &payload).unwrap();
        assert_eq!(first.bound, 1);
        assert_eq!(second.bound, 1);
        assert_eq!(resolved_ids(&first), vec!["light.a"]);
        // Каталог собран заново для каждого действия: два вызова ha_search.
        assert_eq!(mock.search_count(), 2);
    }
}
