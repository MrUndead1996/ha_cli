use crate::client::Client;
use crate::errors::{ErrorType, HaCliError};
use crate::security::expanduser;
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_CACHE_TTL: u64 = 300;

#[derive(Debug, Clone, Default)]
pub struct Tool {
    pub basename: String,
    pub mcp_name: String,
    pub input_schema: Json,
}

impl Tool {
    /// Перенос `Tool.basename_of`.
    pub fn basename_of(mcp_name: &str) -> &str {
        mcp_name.rsplit("__").next().unwrap_or(mcp_name)
    }
}

#[derive(Debug, Default, Clone)]
pub struct ToolMapping {
    pub tools: HashMap<String, Tool>,
    pub ambiguous: HashSet<String>,
    pub source_tools: Vec<Json>,
}

/// Перенос `discover_tools`.
pub fn discover_tools(tools: Vec<Json>) -> ToolMapping {
    let mut mapping = ToolMapping {
        source_tools: tools,
        ..Default::default()
    };
    for tool in &mapping.source_tools {
        let mcp_name = tool.get("name").and_then(Json::as_str).unwrap_or("");
        let basename = Tool::basename_of(mcp_name);
        let duplicate = mapping
            .tools
            .get(basename)
            .is_some_and(|existing| existing.mcp_name != mcp_name);
        if duplicate {
            mapping.ambiguous.insert(basename.to_string());
            mapping.tools.remove(basename);
            continue;
        }
        if !mapping.ambiguous.contains(basename) {
            mapping.tools.insert(
                basename.to_string(),
                Tool {
                    basename: basename.to_string(),
                    mcp_name: mcp_name.to_string(),
                    input_schema: tool.get("inputSchema").cloned().unwrap_or(json!({})),
                },
            );
        }
    }
    mapping
}

/// Перенос `get_tool`.
pub fn get_tool<'a>(mapping: &'a ToolMapping, basename: &str) -> Result<&'a Tool, HaCliError> {
    match mapping.tools.get(basename) {
        Some(tool) => Ok(tool),
        None => {
            if mapping.ambiguous.contains(basename) {
                Err(HaCliError::new(
                    ErrorType::AmbiguousTool,
                    format!("ambiguous tool: {basename}"),
                ))
            } else {
                Err(HaCliError::new(
                    ErrorType::ToolNotFound,
                    format!("tool not found: {basename}"),
                ))
            }
        }
    }
}

/// Перенос `save_cache`: запись JSON с правами 0600. Файл привязывается
/// к endpoint через поле `endpoint` (идентификатор без секретов) — кэш,
/// записанный для другого сервера, никогда не будет прочитан.
pub fn save_cache(
    mapping: &ToolMapping,
    cache_path: &str,
    endpoint_id: &str,
) -> Result<(), HaCliError> {
    let path = expanduser(cache_path);
    let path = Path::new(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            HaCliError::new(ErrorType::Generic, format!("cannot create cache dir: {e}"))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            HaCliError::new(ErrorType::Generic, format!("cannot write tool cache: {e}"))
        })?;
    // Явно выставляем 0600 (аналог os.fchmod после os.open).
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| {
            HaCliError::new(ErrorType::Generic, format!("cannot chmod tool cache: {e}"))
        })?;
    let payload = json!({"endpoint": endpoint_id, "tools": mapping.source_tools});
    write!(file, "{payload}").map_err(|e| {
        HaCliError::new(ErrorType::Generic, format!("cannot write tool cache: {e}"))
    })?;
    Ok(())
}

/// Перенос `load_cache` с привязкой к endpoint: файл без поля `endpoint`
/// (legacy-кэш до разделения) либо с ЧУЖИМ идентификатором — invalid
/// (fail-safe: такой кэш не подставляется, будет refresh).
pub fn load_cache(cache_path: &str, endpoint_id: &str) -> Result<ToolMapping, HaCliError> {
    let path = expanduser(cache_path);
    let text = std::fs::read_to_string(&path)
        .map_err(|_| HaCliError::new(ErrorType::Generic, "invalid tool cache".to_string()))?;
    let data: Json = serde_json::from_str(&text)
        .map_err(|_| HaCliError::new(ErrorType::Generic, "invalid tool cache".to_string()))?;
    if data.get("endpoint").and_then(Json::as_str) != Some(endpoint_id) {
        return Err(HaCliError::new(
            ErrorType::Generic,
            "invalid tool cache".to_string(),
        ));
    }
    let Some(tools) = data.get("tools").and_then(Json::as_array) else {
        return Err(HaCliError::new(
            ErrorType::Generic,
            "invalid tool cache".to_string(),
        ));
    };
    Ok(discover_tools(tools.clone()))
}

/// Идентификатор endpoint для изоляции кэша схем инструментов
/// (docs/mcp_migration.md, этап 5, подпункт 1): SHA-256 от ПОЛНОГО URL,
/// включая секретный webhook path. Обоснование выбора дайджеста: URL сам
/// является секретом — детерминированный «дешёвый» отпечаток (длина, хост,
/// CRC и т.п.) позволил бы перебором восстановить приватный URL; SHA-256
/// не раскрывает URL и не содержит секрета ни в каком виде, поэтому
/// безопасен и в имени файла, и в содержимом кэша. Разные ha-mcp
/// endpoint'ы (другой webhook/хост) получают разные файлы и не видят кэш
/// друг друга.
pub fn endpoint_cache_id(mcp_url: &str) -> String {
    let digest = Sha256::digest(mcp_url.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("hamcp-{hex}")
}

/// Путь кэша для конкретного endpoint: `~/.cache/ha-cli/tools-<id>.json`.
/// Имя файла не содержит URL — только несекретный дайджест.
pub fn default_cache_path_for(endpoint_id: &str) -> String {
    let cache_home = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| "~/.cache".to_string());
    let expanded = expanduser(&cache_home);
    Path::new(&expanded)
        .join("ha-cli")
        .join(format!("tools-{endpoint_id}.json"))
        .to_string_lossy()
        .into_owned()
}

/// Перенос `is_stale_tool_result`.
pub fn is_stale_tool_result(result: &Json) -> bool {
    let is_error = match result.get("isError") {
        Some(Json::Bool(b)) => *b,
        Some(other) => !other.is_null(),
        None => false,
    };
    if !is_error {
        return false;
    }
    let Some(content) = result.get("content").and_then(Json::as_array) else {
        return false;
    };
    for item in content {
        if item.get("type").and_then(Json::as_str) == Some("text") {
            let text = item.get("text").and_then(Json::as_str).unwrap_or("");
            return text.contains("not found") && text.contains("Tool ");
        }
    }
    false
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Перенос `ToolDiscovery`. Клиент передаётся в методы (borrow checker
/// не позволяет удерживать `&mut Client` в поле).
pub struct ToolDiscovery {
    pub cache_path: String,
    pub endpoint_id: String,
    pub cache_ttl: u64,
    mapping: Option<ToolMapping>,
    from_cache: bool,
}

impl ToolDiscovery {
    /// Кэш для конкретного endpoint: по дайджесту полного URL. Разные
    /// серверы никогда не делят один файл.
    pub fn for_endpoint(mcp_url: &str) -> Self {
        let endpoint_id = endpoint_cache_id(mcp_url);
        Self {
            cache_path: default_cache_path_for(&endpoint_id),
            endpoint_id,
            cache_ttl: DEFAULT_CACHE_TTL,
            mapping: None,
            from_cache: false,
        }
    }

    pub fn from_cache(&self) -> bool {
        self.from_cache
    }

    /// Перенос `tools`.
    pub fn tools(&mut self, client: &mut Client) -> Result<ToolMapping, HaCliError> {
        if let Some(mapping) = &self.mapping {
            return Ok(mapping.clone());
        }
        if let Some(cached) = self.load_fresh_cache() {
            self.from_cache = true;
            self.mapping = Some(cached.clone());
            return Ok(cached);
        }
        self.refresh(client)
    }

    /// Перенос `tools(refresh=True)`.
    pub fn tools_refresh(&mut self, client: &mut Client) -> Result<ToolMapping, HaCliError> {
        self.refresh(client)
    }

    /// Перенос `refresh`.
    pub fn refresh(&mut self, client: &mut Client) -> Result<ToolMapping, HaCliError> {
        let mapping = discover_tools(client.tools_list()?);
        save_cache(&mapping, &self.cache_path, &self.endpoint_id)?;
        self.from_cache = false;
        self.mapping = Some(mapping.clone());
        Ok(mapping)
    }

    /// Перенос `get_tool`. Возвращает копию `Tool` (владение нужно,
    /// чтобы обновить кэш между двумя поисками).
    pub fn get_tool(&mut self, client: &mut Client, basename: &str) -> Result<Tool, HaCliError> {
        let ambiguous = self.tools(client)?.ambiguous.contains(basename);
        if ambiguous {
            return Err(HaCliError::new(
                ErrorType::AmbiguousTool,
                format!("ambiguous tool: {basename}"),
            ));
        }
        if self.mapping.as_ref().unwrap().tools.contains_key(basename) {
            return Ok(get_tool(self.mapping.as_ref().unwrap(), basename)?.clone());
        }
        if !self.from_cache {
            return Err(HaCliError::new(
                ErrorType::ToolNotFound,
                format!("tool not found: {basename}"),
            ));
        }
        let mapping = self.refresh(client)?;
        get_tool(&mapping, basename).cloned()
    }

    /// Перенос `_load_fresh_cache`.
    fn load_fresh_cache(&self) -> Option<ToolMapping> {
        let path = expanduser(&self.cache_path);
        let path = Path::new(&path);
        let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now_secs().saturating_sub(mtime_secs) > self.cache_ttl {
            return None;
        }
        load_cache(&self.cache_path, &self.endpoint_id).ok()
    }
}
