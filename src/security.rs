use serde_json::Value as Json;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const REDACTED: &str = "[REDACTED]";

const ENTITY_KEY_NORMALIZED: &[&str] = &[
    "entityid",
    "entityids",
    "entitiesid",
    "entitiesids",
    "targetentityid",
    "targetentityids",
];

#[derive(Clone)]
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

/// Перенос `stat.filemode` для тех случаев, где режим попадает в сообщение об ошибке.
pub fn filemode(mode: u32) -> String {
    let type_char = match mode & 0o170000 {
        0o140000 => 's',
        0o120000 => 'l',
        0o100000 => '-',
        0o060000 => 'b',
        0o040000 => 'd',
        0o020000 => 'c',
        0o010000 => 'p',
        _ => '?',
    };
    let triad = |base: u32| -> String {
        ['r', 'w', 'x']
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                if mode & (1 << (base + 2 - i as u32)) != 0 {
                    *ch
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!("{type_char}{}{}{}", triad(6), triad(3), triad(0))
}

pub fn require_secure_mode(st_mode: u32, what: &str) -> Result<(), String> {
    if !is_secure_mode(st_mode) {
        return Err(format!(
            "{what} has insecure permissions ({}); only owner read/write is allowed (0600)",
            filemode(st_mode)
        ));
    }
    Ok(())
}

/// Перенос `security.read_token_file`:
/// O_NOFOLLOW|O_NONBLOCK, fstat (regular file + 0600), read+trim, пустота — ошибка.
pub fn read_token_file(path: &str) -> Result<String, String> {
    let full = expanduser(path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(&full)
        .map_err(|e| format!("cannot open token file: {}", strerror(&e)))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot stat token file: {}", strerror(&e)))?;
    if !meta.is_file() {
        return Err("token file is not a regular file".to_string());
    }
    let mode = meta.permissions().mode();
    require_secure_mode(mode, "token file")?;
    let mut contents = String::new();
    use std::io::Read;
    (&file)
        .read_to_string(&mut contents)
        .map_err(|e| format!("cannot read token file: {}", strerror(&e)))?;
    let token = contents.trim().to_string();
    if token.is_empty() {
        return Err("token file is empty".to_string());
    }
    Ok(token)
}

// Linux open(2) flags (mirrored from libc to avoid a direct dependency).
const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;

fn strerror(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code).to_string(),
        None => e.to_string(),
    }
}

/// Перенос `os.path.expanduser` для путей, начинающихся с `~`.
pub fn expanduser(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

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
