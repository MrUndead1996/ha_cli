use crate::errors::HaCliError;
use serde_json::Value as Json;

pub const DEFAULT_CACHE_TTL: u64 = 300;

#[derive(Debug, Clone, Default)]
pub struct Tool {
    pub basename: String,
    pub mcp_name: String,
    pub input_schema: Json,
}

impl Tool {
    pub fn basename_of(mcp_name: &str) -> &str {
        mcp_name.rsplit("__").next().unwrap_or(mcp_name)
    }
}

#[derive(Debug, Default)]
pub struct ToolMapping {
    pub tools: std::collections::HashMap<String, Tool>,
    pub ambiguous: std::collections::HashSet<String>,
    pub source_tools: Vec<Json>,
}

// TODO(phase-2): перенос ha_cli/discovery.py:
//  - discover_tools / get_tool / ToolDiscovery (кэш ~/.cache/ha-cli/tools.json,
//    TTL 300 c, сохранение с правами 0600)
//  - is_stale_tool_result
pub fn discover_tools(tools: Vec<Json>) -> ToolMapping {
    let _ = HaCliError::new(crate::errors::ErrorType::Generic, "");
    ToolMapping {
        source_tools: tools,
        ..Default::default()
    }
}
