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
    pub token: String,
    pub timeout: u64,
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
    // Секретный URL сам является способом авторизации: токен не обязателен.
    if token.is_none() && mcp_url.is_none() {
        return Err(HaCliError::new(
            ErrorType::Configuration,
            "Home Assistant token is not configured",
        ));
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
