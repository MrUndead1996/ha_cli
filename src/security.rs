use serde_json::Value as Json;

pub const REDACTED: &str = "[REDACTED]";

const ENTITY_KEY_NORMALIZED: &[&str] = &[
    "entityid",
    "entityids",
    "entitiesid",
    "entitiesids",
    "targetentityid",
    "targetentityids",
];

pub struct Secrets {
    values: Vec<String>,
}

impl Secrets {
    pub fn new() -> Self {
        Self { values: Vec::new() }
    }

    pub fn register(&mut self, secret: &str) {
        if !secret.is_empty() && !self.values.iter().any(|s| s == secret) {
            self.values.push(secret.to_string());
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut text = text.to_string();
        for secret in &self.values {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), REDACTED);
            }
        }
        text
    }
}

impl Default for Secrets {
    fn default() -> Self {
        Self::new()
    }
}

pub fn is_secure_mode(mode: u32) -> bool {
    mode & 0o077 == 0
}

// TODO(phase-1): read_token_file (O_NOFOLLOW, fstat, 0600),
// require_secure_mode, validate_entity_payload — перенос ha_cli/security.py

pub fn normalize_entity_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub fn is_entity_key(key: &str) -> bool {
    ENTITY_KEY_NORMALIZED.contains(&normalize_entity_key(key).as_str())
}

pub fn looks_like_entity_id(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    let mut parts = v.splitn(2, '.');
    let domain = parts.next().unwrap_or_default();
    let object = parts.next().unwrap_or_default();
    !domain.is_empty()
        && !object.is_empty()
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && object
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub fn validate_entity_payload(payload: &Json) -> Result<(), String> {
    validate_node(payload, "payload")
}

fn validate_node(node: &Json, path: &str) -> Result<(), String> {
    match node {
        Json::Object(map) => {
            for (key, value) in map {
                if is_entity_key(key) {
                    return Err(format!(
                        "{path}.{key}: direct entity ID targeting is not allowed; \
                         use semantic area/domain/name instead"
                    ));
                }
                validate_node(value, &format!("{path}.{key}"))?;
            }
        }
        Json::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                validate_node(value, &format!("{path}[{index}]"))?;
            }
        }
        Json::String(s) if looks_like_entity_id(s) => {
            return Err(format!(
                "{path}: entity ID values are not allowed; \
                 use semantic area/domain/name instead"
            ));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_entity_keys() {
        assert!(is_entity_key("entity_id"));
        assert!(is_entity_key("TargetEntityIDs"));
        assert!(!is_entity_key("area"));
    }

    #[test]
    fn detects_entity_id_values() {
        assert!(looks_like_entity_id("light.kitchen"));
        assert!(!looks_like_entity_id("kitchen"));
    }

    #[test]
    fn rejects_entity_id_payload() {
        let err = validate_entity_payload(&json!({"name": "light.kitchen"}));
        assert!(err.is_err());
    }
}
