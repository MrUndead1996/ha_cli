use serde_json::Value as Json;

#[derive(Debug, Clone, Default)]
pub struct Entity {
    pub area: String,
    pub domain: String,
    pub name: String,
    pub state: Option<String>,
    pub capabilities: Json,
    /// Алиасы СУЩНОСТИ из `ha_search` (`aliases` в result_fields). НЕ
    /// являются алиасами области и используются только как дополнительные
    /// точные имена при сопоставлении `name`.
    pub entity_aliases: Vec<String>,
    /// Внутренний идентификатор из `ha_search` (ha-mcp). Используется только
    /// внутри CLI после разрешения семантической цели и никогда не
    /// принимается от пользователя.
    pub entity_id: Option<String>,
}

impl Entity {
    /// Перенос `Entity.from_raw`, адаптированный к ответу `ha_search`
    /// (ha-mcp): `friendly_name`, `entity_id` и `aliases` в виде массива
    /// (алиасы СУЩНОСТИ, а не области).
    pub fn from_raw(raw: &Json) -> Option<Self> {
        let obj = raw.as_object()?;
        let name = obj
            .get("friendly_name")
            .or_else(|| obj.get("name"))
            .and_then(Json::as_str)?;
        if name.is_empty() {
            return None;
        }
        let domain = match obj.get("domain").and_then(Json::as_str) {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => "unknown".to_string(),
        };
        let state = obj
            .get("state")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let capabilities = obj
            .get("capabilities")
            .filter(|c| c.is_object())
            .cloned()
            .unwrap_or_else(|| Json::Object(Default::default()));
        let entity_id = obj
            .get("entity_id")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let entity_aliases: Vec<String> = obj
            .get("aliases")
            .and_then(Json::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Json::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let area = obj
            .get("area")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("Unknown")
            .to_string();
        Some(Self {
            area,
            domain,
            name: name.to_string(),
            state,
            capabilities,
            entity_aliases,
            entity_id,
        })
    }
}
