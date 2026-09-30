use crate::config::Config;
use crate::errors::{ErrorType, HaCliError};
use serde_json::{json, Value as Json};

pub const ASSIST_MCP_ENDPOINT: &str = "/api/mcp/assist";
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Точка подмены транспорта в тестах (аналог httpx.BaseTransport).
pub trait Transport {
    fn post(&mut self, payload: &Json) -> Result<HttpResponse, HaCliError>;
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Json,
}

pub struct Client {
    pub config: Config,
    next_id: u64,
    initialized: bool,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

// TODO(phase-1): перенос ha_cli/client.py:
//  - HttpTransport на reqwest::blocking::Client (rustls),
//    заголовки Authorization/Content-Type/Accept, Mcp-Session-Id,
//    Mcp-Protocol-Version после initialize
//  - _call / _notify / initialize / tools_list / tools_call
impl Client {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            next_id: 0,
            initialized: false,
            session_id: None,
            protocol_version: None,
        }
    }

    pub fn tools_list(&mut self) -> Result<Vec<Json>, HaCliError> {
        let _ = (&self.next_id, &self.session_id, &self.protocol_version);
        if !self.initialized {
            return Err(HaCliError::new(
                ErrorType::HaApi,
                "client: initialize not implemented yet",
            ));
        }
        Ok(Vec::new())
    }

    pub fn tools_call(
        &mut self,
        _name: &str,
        _arguments: &Json,
    ) -> Result<Json, HaCliError> {
        Err(HaCliError::new(
            ErrorType::HaApi,
            "client: tools_call not implemented yet",
        ))
    }
}

pub fn build_request(id: u64, method: &str, params: Option<&Json>) -> Json {
    let mut payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
    });
    if let Some(params) = params {
        payload["params"] = params.clone();
    }
    payload
}

pub fn build_notification(method: &str, params: Option<&Json>) -> Json {
    let mut payload = json!({"jsonrpc": "2.0", "method": method});
    if let Some(params) = params {
        payload["params"] = params.clone();
    }
    payload
}
