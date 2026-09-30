use serde_json::Value as Json;

#[derive(Debug, Clone, Default)]
pub struct Entity {
    pub area: String,
    pub domain: String,
    pub name: String,
    pub state: Option<String>,
    pub capabilities: Json,
    /// Алиасы ОБЛАСТИ ( Assist: дописаны в `area` через запятую;
    /// ha-mcp: не предоставляются на этом этапе — остаются пустыми).
    /// Используются только для сопоставления `area` и вывода `area_aliases`.
    pub aliases: Vec<String>,
    /// Алиасы СУЩНОСТИ из `ha_search` (`aliases` в result_fields). Хранятся
    /// отдельно и НЕ попадают в `area_aliases` и сопоставление области;
    /// могут использоваться для поиска по имени сущности позднее.
    pub entity_aliases: Vec<String>,
    /// Внутренний идентификатор из `ha_search` (ha-mcp). Используется только
    /// внутри CLI после разрешения семантической цели и никогда не
    /// принимается от пользователя.
    pub entity_id: Option<String>,
}

impl Entity {
    /// Перенос `Entity.from_raw` из ha_cli/models.py, адаптированный к
    /// ответу `ha_search` (ha-mcp): `friendly_name`, `entity_id` и
    /// `aliases` в виде массива. Прежний формат Assist (`name`, область
    /// с алиасами через запятую в `area`) сохранён.
    pub fn from_raw(raw: &Json) -> Option<Self> {
        let obj = raw.as_object()?;
        let name = obj
            .get("name")
            .or_else(|| obj.get("friendly_name"))
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
        // ha_search: `aliases` — алиасы СУЩНОСТИ, а не области; алиасы
        // области на этом этапе не предоставляются, поэтому area aliases
        // остаются пустыми (не перетолковываем). Assist: алиасы области
        // дописаны в `area` через запятую, первый элемент — сама область.
        let (area, aliases, entity_aliases) = match obj.get("aliases").and_then(Json::as_array) {
            Some(list) => {
                let area = split_aliases(obj.get("area"))
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "Unknown".to_string());
                let entity_aliases: Vec<String> = list
                    .iter()
                    .filter_map(Json::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect();
                (area, Vec::new(), entity_aliases)
            }
            None => {
                let aliases = split_aliases(obj.get("area"));
                let area = aliases
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Unknown".to_string());
                (area, aliases.into_iter().skip(1).collect(), Vec::new())
            }
        };
        Some(Self {
            area,
            domain,
            name: name.to_string(),
            state,
            capabilities,
            aliases,
            entity_aliases,
            entity_id,
        })
    }
}

fn split_aliases(value: Option<&Json>) -> Vec<String> {
    let Some(s) = value.and_then(Json::as_str) else {
        return Vec::new();
    };
    s.split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect()
}
