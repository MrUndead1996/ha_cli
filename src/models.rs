use serde_json::Value as Json;

#[derive(Debug, Clone, Default)]
pub struct Entity {
    pub area: String,
    pub domain: String,
    pub name: String,
    pub state: Option<String>,
    pub capabilities: Json,
    pub aliases: Vec<String>,
}

impl Entity {
    /// Перенос `Entity.from_raw` из ha_cli/models.py.
    pub fn from_raw(raw: &Json) -> Option<Self> {
        let obj = raw.as_object()?;
        let name = obj.get("name")?.as_str()?;
        if name.is_empty() {
            return None;
        }
        let domain = match obj.get("domain").and_then(Json::as_str) {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => "unknown".to_string(),
        };
        let aliases = split_aliases(obj.get("area"));
        let area = aliases
            .first()
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());
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
        Some(Self {
            area,
            domain,
            name: name.to_string(),
            state,
            capabilities,
            aliases: aliases.into_iter().skip(1).collect(),
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
