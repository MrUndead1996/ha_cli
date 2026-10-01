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

/// Семантические селекторы, общие для всех интентов.
const SELECTOR_KEYS: &[&str] = &["area", "domain", "name"];

/// Проверка, что значение является ЦЕЛОЧИСЛЕННЫМ JSON-числом в диапазоне
/// `[lo, hi]`. Принимается только `serde_json::Number` с целым
/// представлением (`as_i64`/`as_u64`): дробные литералы вроде `50.0`
/// отклоняются как другой JSON-тип, без cast через f64.
fn whole_number_in_range(value: &Json, lo: i64, hi: i64) -> Option<i64> {
    let number = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))?;
    if number < lo || number > hi {
        return None;
    }
    Some(number)
}

fn invalid(key: &str, requirement: &str) -> String {
    format!("{key}: {requirement}")
}

/// Проверка одного параметра сервисных данных по интент-специфичному
/// allowlist с типами и диапазонами (docs/mcp_migration.md, этап 4.1).
/// Значение семантики: `brightness` — 0..100 как в существующем скилле,
/// на ha-mcp (этап 4.2) отображается в подтверждённое поле
/// `brightness_pct` сервиса `light.turn_on`.
fn validate_service_param(intent: &str, key: &str, value: &Json) -> Result<(), String> {
    match (intent, key) {
        ("HassLightSet", "brightness") => {
            if whole_number_in_range(value, 0, 100).is_none() {
                return Err(invalid(
                    key,
                    "must be a whole number from 0 to 100 (percent)",
                ));
            }
        }
        ("HassLightSet", "color_temp_kelvin") => {
            // Поле подтверждено живым ha_list_services (light.turn_on,
            // этап 1); диапазон — разумная граница вокруг рабочих значений
            // бытовых ламп; точный min/max атрибутов конкретной лампы
            // не проверяется до сети.
            if whole_number_in_range(value, 1000, 12000).is_none() {
                return Err(invalid(
                    key,
                    "must be a whole number of kelvin from 1000 to 12000",
                ));
            }
        }
        ("HassLightSet", "transition") => {
            let finite = value.as_f64().filter(|v| v.is_finite());
            match finite {
                Some(seconds) if (0.0..=300.0).contains(&seconds) => {}
                _ => {
                    return Err(invalid(
                        key,
                        "must be a finite number of seconds from 0 to 300",
                    ))
                }
            }
        }
        ("HassLightSet", "rgb_color") => {
            let channels = value.as_array().filter(|items| items.len() == 3);
            let valid = channels.is_some_and(|items| {
                items
                    .iter()
                    .all(|channel| whole_number_in_range(channel, 0, 255).is_some())
            });
            if !valid {
                return Err(invalid(
                    key,
                    "must be an array of three whole numbers from 0 to 255",
                ));
            }
        }
        ("HassLightSet", "effect") => {
            if value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                return Err(invalid(key, "must be a non-empty string"));
            }
        }
        ("HassSetPosition", "position") => {
            if whole_number_in_range(value, 0, 100).is_none() {
                return Err(invalid(
                    key,
                    "must be a whole number from 0 to 100 (percent)",
                ));
            }
        }
        (_, unexpected) => {
            let allowed = match intent {
                "HassLightSet" => {
                    "area, domain, name, brightness, color_temp_kelvin, transition, \
                     rgb_color, effect"
                }
                "HassSetPosition" => "area, domain, name, position",
                "HassTurnOn" | "HassTurnOff" | "HassGetState" => "area, domain, name",
                other => return Err(format!("unknown intent '{other}'")),
            };
            return Err(format!(
                "{unexpected}: parameter is not allowed for {intent}; allowed keys: {allowed}"
            ));
        }
    }
    Ok(())
}

/// Строгая валидация payload действия ДО любых сетевых вызовов
/// (docs/mcp_migration.md, этап 4.1). Включает рекурсивный запрет
/// `entity_id` (`validate_entity_payload`), проверку неизвестных ключей по
/// интент-специфичному allowlist, типов и диапазонов значений, а также
/// допустимых сочетаний параметров:
/// - селекторы `area` / `domain` / `name` обязаны быть непустыми строками,
///   если присутствуют (не-строковый селектор — ошибка, а не молчаливое
///   игнорирование);
/// - `color_temp_kelvin` и `rgb_color` взаимно исключающие (разные цветовые
///   режимы света);
/// - `HassLightSet` требует домен `light`, `HassSetPosition` — домен `cover`,
///   если домен указан явно (проверка ДО сети, по селектору).
pub fn validate_intent_payload(intent: &str, payload: &Json) -> Result<(), String> {
    validate_entity_payload(payload)?;
    let object = payload
        .as_object()
        .ok_or_else(|| "payload must be a JSON object".to_string())?;
    for key in SELECTOR_KEYS {
        if let Some(value) = object.get(*key) {
            if value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_none()
            {
                return Err(invalid(key, "selector must be a non-empty string"));
            }
        }
    }
    for (key, value) in object {
        if !SELECTOR_KEYS.contains(&key.as_str()) {
            validate_service_param(intent, key, value)?;
        }
    }
    // `HassSetPosition` без `position` бессмысленен: параметр обязателен.
    if intent == "HassSetPosition" && !object.contains_key("position") {
        return Err(invalid(
            "position",
            "is required for HassSetPosition (whole number from 0 to 100)",
        ));
    }
    // Взаимно исключающие параметры света: цветовая температура задаёт
    // цветовой режим color_temp, RGB — rgb; одновременно они не передаются.
    if intent == "HassLightSet"
        && object.contains_key("color_temp_kelvin")
        && object.contains_key("rgb_color")
    {
        return Err(invalid(
            "color_temp_kelvin",
            "and rgb_color are mutually exclusive color modes; specify only one",
        ));
    }
    // Проверка домена по селектору до сети: параметры не имеют смысла для
    // других доменов, и запрос не должен доходить до разрешения/вызова.
    let domain = object
        .get("domain")
        .and_then(Json::as_str)
        .map(|domain| domain.trim().to_lowercase());
    if let Some(domain) = domain {
        let expected = match intent {
            "HassLightSet" => "light",
            "HassSetPosition" => "cover",
            _ => return Ok(()),
        };
        if domain != expected {
            return Err(format!(
                "domain '{domain}' is not supported for {intent}; expected '{expected}'"
            ));
        }
    }
    Ok(())
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

    #[test]
    fn intent_payload_accepts_valid_combinations() {
        for (intent, payload) in [
            ("HassTurnOn", json!({"area": "kitchen"})),
            ("HassTurnOff", json!({"area": "bedroom", "domain": "light"})),
            ("HassGetState", json!({"name": "Lamp"})),
            (
                "HassLightSet",
                json!({"area": "кухня", "name": "Лампа", "brightness": 50}),
            ),
            (
                "HassLightSet",
                json!({"area": "кухня", "color_temp_kelvin": 3000, "transition": 1.5}),
            ),
            (
                "HassLightSet",
                json!({"name": "Lamp", "rgb_color": [255, 0, 128]}),
            ),
            (
                "HassSetPosition",
                json!({"area": "bedroom", "position": 70}),
            ),
        ] {
            assert!(
                validate_intent_payload(intent, &payload).is_ok(),
                "{intent} {payload}"
            );
        }
    }

    #[test]
    fn intent_payload_rejects_unknown_keys() {
        for (intent, key) in [
            ("HassTurnOn", "entity_id"),
            ("HassTurnOn", "data"),
            ("HassGetState", "fields"),
            ("HassTurnOff", "brightness"),
            ("HassLightSet", "position"),
            ("HassSetPosition", "brightness"),
        ] {
            let mut payload = serde_json::Map::new();
            payload.insert(key.to_string(), json!("x"));
            let err = validate_intent_payload(intent, &Json::Object(payload)).unwrap_err();
            assert!(err.contains("not allowed"), "{intent}: {err}");
        }
    }

    #[test]
    fn intent_payload_rejects_bad_types_and_ranges() {
        for (intent, payload) in [
            ("HassLightSet", json!({"brightness": 101})),
            ("HassLightSet", json!({"brightness": -1})),
            ("HassLightSet", json!({"brightness": "50"})),
            ("HassLightSet", json!({"brightness": 50.5})),
            // Дробный JSON-литерал — другой тип, отклоняется даже при
            // целом значении (контракт: целое число, не float).
            ("HassLightSet", json!({"brightness": 50.0})),
            ("HassLightSet", json!({"color_temp_kelvin": 3000.0})),
            ("HassLightSet", json!({"rgb_color": [1.0, 2, 3]})),
            ("HassSetPosition", json!({"position": 70.0})),
            ("HassLightSet", json!({"color_temp_kelvin": 999})),
            ("HassLightSet", json!({"color_temp_kelvin": "3000"})),
            ("HassLightSet", json!({"transition": -0.1})),
            ("HassLightSet", json!({"rgb_color": [255, 0]})),
            ("HassLightSet", json!({"rgb_color": [255, 0, 999]})),
            ("HassLightSet", json!({"effect": ""})),
            ("HassSetPosition", json!({"position": 101})),
            ("HassSetPosition", json!({"position": "70"})),
        ] {
            assert!(
                validate_intent_payload(intent, &payload).is_err(),
                "{intent} {payload}"
            );
        }
    }

    #[test]
    fn intent_payload_requires_string_selectors() {
        for payload in [
            json!({"area": 123}),
            json!({"area": "kitchen", "name": 42}),
            json!({"area": ""}),
            json!({"domain": ["light"]}),
        ] {
            assert!(
                validate_intent_payload("HassTurnOn", &payload).is_err(),
                "{payload}"
            );
        }
    }

    #[test]
    fn intent_payload_rejects_incompatible_combinations() {
        let both = json!({"area": "kitchen", "color_temp_kelvin": 3000, "rgb_color": [1, 2, 3]});
        let err = validate_intent_payload("HassLightSet", &both).unwrap_err();
        assert!(err.contains("mutually exclusive"));
        let err =
            validate_intent_payload("HassLightSet", &json!({"domain": "switch"})).unwrap_err();
        assert!(err.contains("expected 'light'"));
        let err = validate_intent_payload(
            "HassSetPosition",
            &json!({"domain": "light", "position": 50}),
        )
        .unwrap_err();
        assert!(err.contains("expected 'cover'"));
        assert!(validate_intent_payload("HassLightSet", &json!({"domain": "Light"})).is_ok());
    }

    #[test]
    fn intent_payload_still_bans_entity_id_everywhere() {
        let err =
            validate_intent_payload("HassLightSet", &json!({"data": {"entity_ids": ["x.y"]}}))
                .unwrap_err();
        assert!(err.contains("entity"));
    }
}
