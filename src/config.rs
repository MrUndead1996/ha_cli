use crate::errors::{ErrorType, HaCliError};
use crate::security::{self, Secrets};
use serde_json::Value as Json;
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone)]
pub struct Config {
    /// Базовый `HA_URL`; нужен только для прежнего Assist endpoint.
    /// При настроенном `mcp_url` (mcp-only) может отсутствовать.
    pub url: Option<String>,
    /// Полный MCP endpoint URL (webhook `/api/webhook/<secret>` или прямой
    /// `/private_<secret>`). Путь URL является секретом. `None` — прежний
    /// Assist endpoint (`HA_URL` + `/api/mcp/assist`).
    pub mcp_url: Option<String>,
    /// Способ авторизации настроенного `mcp_url`. `None` (по умолчанию) —
    /// секретный URL авторизуется сам по себе, токен не отправляется.
    /// `HaAuth` — явно разрешённая отправка `HA_TOKEN` Bearer-заголовком на
    /// `HA_MCP_URL` (webhook `ha_auth`). Прежний Assist endpoint всегда
    /// получает Bearer как раньше.
    pub mcp_auth: McpAuth,
    pub token: String,
    pub timeout: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpAuth {
    /// По умолчанию: токен не отправляется на `HA_MCP_URL`.
    #[default]
    None,
    /// Явный opt-in: отправлять `HA_TOKEN` Bearer-заголовком на `HA_MCP_URL`.
    HaAuth,
}

impl McpAuth {
    pub fn parse(value: &str) -> Result<Self, HaCliError> {
        match value {
            "ha_auth" => Ok(Self::HaAuth),
            "none" => Ok(Self::None),
            _ => Err(HaCliError::new(
                ErrorType::Configuration,
                // Значение не отражается: оно может прийти из окружения
                // и содержать секрет.
                "Invalid mcp_auth value: expected `ha_auth` or `none`",
            )),
        }
    }
}

pub const DEFAULT_TIMEOUT: u64 = 5;
const DEFAULT_CONFIG_PATH: &str = "~/.config/ha-cli/config.toml";

/// Перенос `load_config(config_path=DEFAULT_CONFIG_PATH, ...)`.
pub fn load_config(
    cli_url: Option<&str>,
    cli_token: Option<&str>,
    secrets: &mut Secrets,
) -> Result<Config, HaCliError> {
    load_config_from(DEFAULT_CONFIG_PATH, cli_url, cli_token, secrets)
}

/// Перенос `load_config(config_path, cli_url, cli_token)`.
pub fn load_config_from(
    config_path: &str,
    cli_url: Option<&str>,
    cli_token: Option<&str>,
    secrets: &mut Secrets,
) -> Result<Config, HaCliError> {
    let path = security::expanduser(config_path);
    let data = read_toml(&path)?;

    if data.get("token").is_some_and(|t| !t.is_null())
        || data
            .get("mcp_url")
            .is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    {
        require_secure_config_file(&path)?;
    }

    let mcp_url = std::env::var("HA_MCP_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            data.get("mcp_url")
                .and_then(Json::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        });
    // Путь и секретная часть URL (webhook `/api/webhook/<secret>` или прямой
    // `/private_<secret>`) подлежат редактированию в сообщениях об ошибках
    // и debug/diagnostic JSON — даже если отражён только path или секрет.
    if let Some(ref secret_url) = mcp_url {
        register_mcp_secrets(secrets, secret_url);
    }

    let url = cli_url
        .map(str::to_string)
        .or_else(|| std::env::var("HA_URL").ok().filter(|v| !v.is_empty()))
        .or_else(|| data.get("url").and_then(Json::as_str).map(str::to_string))
        .filter(|v| !v.is_empty());
    // Полный MCP URL самодостаточен: HA_URL требуется только для Assist.
    if url.is_none() && mcp_url.is_none() {
        return Err(HaCliError::new(
            ErrorType::Configuration,
            "Home Assistant URL is not configured",
        ));
    }

    let token_result: Result<Option<String>, HaCliError> = (|| {
        if let Some(t) = cli_token {
            return Ok(Some(t.to_string()));
        }
        if let Ok(t) = std::env::var("HA_TOKEN") {
            if !t.is_empty() {
                return Ok(Some(t));
            }
        }
        let token_file = std::env::var("HA_TOKEN_FILE")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                data.get("token_file")
                    .and_then(Json::as_str)
                    .map(str::to_string)
            });
        if let Some(tf) = token_file {
            return security::read_token_file(&tf).map(Some).map_err(|e| {
                // В Python cli.py ValueError/OSError из load_config
                // превращаются в ConfigurationError (exit 3).
                HaCliError::new(ErrorType::Configuration, e)
            });
        }
        Ok(data
            .get("token")
            .and_then(Json::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string))
    })();
    let token = token_result?.filter(|t| !t.is_empty());
    // Секретный URL сам является способом авторизации: токен не обязателен,
    // если не выбран явный режим `ha_auth`.
    if token.is_none() && mcp_url.is_none() {
        return Err(HaCliError::new(
            ErrorType::Configuration,
            "Home Assistant token is not configured",
        ));
    }

    // Приоритет: env `HA_MCP_AUTH` > TOML `mcp_auth` > `none` (по умолчанию
    // токен на `HA_MCP_URL` не отправляется). Проверки выполняются до сети.
    let mcp_auth = std::env::var("HA_MCP_AUTH")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            data.get("mcp_auth")
                .and_then(Json::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        });
    let mcp_auth = match mcp_auth.as_deref() {
        Some(value) => McpAuth::parse(value)?,
        None => McpAuth::None,
    };
    if mcp_auth == McpAuth::HaAuth {
        if mcp_url.is_none() {
            return Err(HaCliError::new(
                ErrorType::Configuration,
                "mcp_auth=ha_auth requires HA_MCP_URL to be configured",
            ));
        }
        if token.is_none() {
            return Err(HaCliError::new(
                ErrorType::Configuration,
                "mcp_auth=ha_auth requires a Home Assistant token",
            ));
        }
    }
    if let Some(t) = &token {
        secrets.register(t);
    }

    let timeout = data
        .get("timeout")
        .and_then(Json::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT);

    Ok(Config {
        url,
        mcp_url,
        mcp_auth,
        token: token.unwrap_or_default(),
        timeout,
    })
}

/// Регистрация секретов MCP URL: полный URL, путь целиком и секретный
/// сегмент пути. Редактирование должно срабатывать и тогда, когда в сообщении
/// отражён только path или только секрет.
pub fn register_mcp_secrets(secrets: &mut Secrets, url: &str) {
    secrets.register(url);
    let Some((_, path)) = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
    else {
        return;
    };
    let path = format!("/{path}");
    secrets.register(&path);
    if let Some(segment) = path.rsplit('/').next() {
        if !segment.is_empty() {
            secrets.register(segment);
        }
    }
}

fn read_toml(path: &str) -> Result<Json, HaCliError> {
    match std::fs::read(path) {
        Ok(bytes) => toml::from_str::<toml::Value>(
            &String::from_utf8(bytes)
                .map_err(|e| HaCliError::new(ErrorType::Configuration, e.to_string()))?,
        )
        .map(toml_to_json)
        .map_err(|e| HaCliError::new(ErrorType::Configuration, e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Json::Null),
        Err(e) => Err(HaCliError::new(ErrorType::Configuration, e.to_string())),
    }
}

fn toml_to_json(value: toml::Value) -> Json {
    use toml::Value as Tv;
    match value {
        Tv::String(s) => Json::String(s),
        Tv::Integer(i) => Json::Number(i.into()),
        Tv::Float(f) => {
            Json::Number(serde_json::Number::from_f64(f).unwrap_or(serde_json::Number::from(0)))
        }
        Tv::Boolean(b) => Json::Bool(b),
        Tv::Datetime(d) => Json::String(d.to_string()),
        Tv::Array(items) => Json::Array(items.into_iter().map(toml_to_json).collect()),
        Tv::Table(table) => Json::Object(
            table
                .into_iter()
                .map(|(k, v)| (k, toml_to_json(v)))
                .collect(),
        ),
    }
}

/// Перенос `_require_secure_config_file`: lstat (не следует по symlink),
/// regular file, права 0600.
fn require_secure_config_file(path: &str) -> Result<(), HaCliError> {
    // Аналог os.lstat: symlink_metadata не следует по symlink.
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| HaCliError::new(ErrorType::Configuration, e.to_string()))?;
    if !meta.is_file() {
        return Err(HaCliError::new(
            ErrorType::Configuration,
            "config file with inline token is not a regular file",
        ));
    }
    security::require_secure_mode(meta.permissions().mode(), "config file with inline token")
        .map_err(|e| HaCliError::new(ErrorType::Configuration, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // Переменные окружения — глобальное состояние процесса: все тесты,
    // меняющие env, делят один мьютекс.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Все ключи env, которые читает load_config_from: каждый тест изолирует
    /// ровно этот набор, чтобы параллельные тесты не влияли друг на друга.
    const TEST_ENV_KEYS: &[&str] = &[
        "HA_URL",
        "HA_TOKEN",
        "HA_TOKEN_FILE",
        "HA_MCP_URL",
        "HA_MCP_AUTH",
    ];

    /// Под одним мьютексом сохраняет предыдущие значения ключей и
    /// восстанавливает их при drop (вместо безусловного удаления).
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let saved = TEST_ENV_KEYS
                .iter()
                .map(|key| (*key, std::env::var(key).ok()))
                .collect();
            for key in TEST_ENV_KEYS {
                unsafe { std::env::remove_var(key) };
            }
            Self { saved, _lock: lock }
        }

        fn set(&self, key: &str, value: &str) {
            unsafe { std::env::set_var(key, value) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, previous) in &self.saved {
                match previous {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    /// Возвращает путь конфига вместе с TempDir: директория живёт до конца
    /// теста и удаляется без утечек.
    fn write_config(body: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        // Конфиг с секретами обязан быть 0600: проверка прав сработает
        // при наличии token/mcp_url в TOML.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms).unwrap();
        let path = path.to_str().unwrap().to_string();
        (dir, path)
    }

    fn mcp_url() -> &'static str {
        "http://ha.local/api/webhook/testsecret123"
    }

    #[test]
    fn default_mcp_auth_is_none() {
        let guard = EnvGuard::new();
        guard.set("HA_MCP_URL", mcp_url());
        let config = load_config_from(
            "/nonexistent/config.toml",
            None,
            Some("tok"),
            &mut Secrets::new(),
        )
        .unwrap();
        assert_eq!(config.mcp_auth, McpAuth::None);
        assert_eq!(config.mcp_url.as_deref(), Some(mcp_url()));
    }

    #[test]
    fn env_ha_auth_is_explicit_opt_in() {
        let guard = EnvGuard::new();
        guard.set("HA_MCP_URL", mcp_url());
        guard.set("HA_MCP_AUTH", "ha_auth");
        let config = load_config_from(
            "/nonexistent/config.toml",
            None,
            Some("tok"),
            &mut Secrets::new(),
        )
        .unwrap();
        assert_eq!(config.mcp_auth, McpAuth::HaAuth);
    }

    #[test]
    fn env_priority_over_toml_mcp_auth() {
        let guard = EnvGuard::new();
        guard.set("HA_MCP_URL", mcp_url());
        guard.set("HA_MCP_AUTH", "none");
        let (_dir, path) = write_config("mcp_auth = \"ha_auth\"\n");
        let config = load_config_from(&path, None, Some("tok"), &mut Secrets::new()).unwrap();
        assert_eq!(config.mcp_auth, McpAuth::None);
    }

    #[test]
    fn toml_mcp_auth_ha_auth_without_env() {
        let _guard = EnvGuard::new();
        let (_dir, path) = write_config(&format!(
            "mcp_url = \"{u}\"\nmcp_auth = \"ha_auth\"\n",
            u = mcp_url()
        ));
        let config = load_config_from(&path, None, Some("tok"), &mut Secrets::new()).unwrap();
        assert_eq!(config.mcp_auth, McpAuth::HaAuth);
    }

    #[test]
    fn invalid_mcp_auth_value_is_configuration_error() {
        let guard = EnvGuard::new();
        guard.set("HA_MCP_URL", mcp_url());
        guard.set("HA_MCP_AUTH", "bearer");
        let err = load_config_from(
            "/nonexistent/config.toml",
            None,
            Some("tok"),
            &mut Secrets::new(),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Configuration));
    }

    #[test]
    fn ha_auth_without_token_is_configuration_error_before_network() {
        let guard = EnvGuard::new();
        guard.set("HA_MCP_URL", mcp_url());
        guard.set("HA_MCP_AUTH", "ha_auth");
        let err = load_config_from("/nonexistent/config.toml", None, None, &mut Secrets::new())
            .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Configuration));
        assert!(err.message.contains("ha_auth"));
    }

    #[test]
    fn ha_auth_without_mcp_url_is_configuration_error() {
        let _guard = EnvGuard::new();
        _guard.set("HA_MCP_AUTH", "ha_auth");
        let err = load_config_from(
            "/nonexistent/config.toml",
            Some("http://ha.local"),
            Some("tok"),
            &mut Secrets::new(),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Configuration));
    }

    #[test]
    fn mcp_url_with_existing_ha_token_does_not_enable_ha_auth() {
        let guard = EnvGuard::new();
        guard.set("HA_MCP_URL", mcp_url());
        guard.set("HA_TOKEN", "existing-token");
        let config =
            load_config_from("/nonexistent/config.toml", None, None, &mut Secrets::new()).unwrap();
        assert_eq!(config.token, "existing-token");
        assert_eq!(config.mcp_auth, McpAuth::None);
    }

    #[test]
    fn assist_config_without_mcp_url_still_requires_token() {
        let _guard = EnvGuard::new();
        let err = load_config_from(
            "/nonexistent/config.toml",
            Some("http://ha.local"),
            None,
            &mut Secrets::new(),
        )
        .unwrap_err();
        assert!(matches!(err.kind, ErrorType::Configuration));
    }

    #[test]
    fn secure_mode_still_required_for_inline_token() {
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \"tok\"\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
        let err =
            load_config_from(path.to_str().unwrap(), None, None, &mut Secrets::new()).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Configuration));
    }
}
