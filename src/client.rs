use crate::config::{Config, McpAuth};
use crate::errors::{ErrorType, HaCliError};
use crate::security::Secrets;
use serde_json::{json, Value as Json};

pub const ASSIST_MCP_ENDPOINT: &str = "/api/mcp/assist";
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Полный endpoint запроса: настроенный MCP URL используется как есть
/// (webhook `/api/webhook/<secret>` или прямой `/private_<secret>`),
/// иначе прежний Assist endpoint на базе `HA_URL`.
pub fn resolve_endpoint(config: &Config) -> String {
    match &config.mcp_url {
        Some(mcp_url) => mcp_url.clone(),
        None => format!(
            "{}{}",
            config
                .url
                .as_deref()
                .unwrap_or_default()
                .trim_end_matches('/'),
            ASSIST_MCP_ENDPOINT
        ),
    }
}

/// Точка подмены транспорта в тестах (аналог httpx.BaseTransport).
pub trait Transport {
    fn post(
        &mut self,
        url: &str,
        payload: &Json,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, HaCliError>;
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub struct Client {
    pub config: Config,
    transport: Box<dyn Transport>,
    secrets: Secrets,
    next_id: u64,
    initialized: bool,
    session_id: Option<String>,
    protocol_version: Option<String>,
}

impl Client {
    pub fn new(config: Config, transport: Box<dyn Transport>, secrets: &Secrets) -> Self {
        Self {
            config,
            transport,
            secrets: secrets.clone(),
            next_id: 0,
            initialized: false,
            session_id: None,
            protocol_version: None,
        }
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        // Bearer передаётся как заголовок запроса (а не default_headers),
        // чтобы подменный транспорт в тестах видел фактическую авторизацию.
        if let Some(auth) = auth_header(&self.config) {
            headers.push(("Authorization".to_string(), auth));
        }
        if let Some(sid) = &self.session_id {
            headers.push(("Mcp-Session-Id".to_string(), sid.clone()));
        }
        if let Some(pv) = &self.protocol_version {
            headers.push(("MCP-Protocol-Version".to_string(), pv.clone()));
        }
        headers
    }

    /// Перенос `Client._post`.
    fn post(&mut self, payload: &Json) -> Result<HttpResponse, HaCliError> {
        let headers = self.extra_headers();
        let response = self
            .transport
            .post(&resolve_endpoint(&self.config), payload, &headers)
            .map_err(|mut err| {
                err.message = self.secrets.redact(&err.message);
                err
            })?;
        if response.status == 401 || response.status == 403 {
            return Err(HaCliError::new(
                ErrorType::Authentication,
                "Home Assistant rejected the access token",
            ));
        }
        if response.status >= 400 {
            return Err(HaCliError::new(
                ErrorType::HaApi,
                format!("Home Assistant returned HTTP {}", response.status),
            ));
        }
        let session_id = response.header("Mcp-Session-Id");
        if let Some(sid) = session_id {
            self.session_id = Some(sid.to_string());
        }
        Ok(response)
    }

    fn parse_body(&self, response: &HttpResponse) -> Result<Json, HaCliError> {
        if response.body.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&response.body).map_err(|_| {
            HaCliError::new(
                ErrorType::HaApi,
                "Home Assistant returned an invalid JSON response",
            )
        })
    }

    /// Перенос `Client._call`.
    pub fn call(&mut self, method: &str, params: Option<&Json>) -> Result<Json, HaCliError> {
        self.next_id += 1;
        let payload = build_request(self.next_id, method, params);
        let response = self.post(&payload)?;
        let data = self.parse_body(&response)?;
        if let Some(err) = data.get("error") {
            let message = err
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("unknown MCP error");
            return Err(HaCliError::new(
                ErrorType::HaApi,
                format!("MCP error: {message}"),
            ));
        }
        match data.get("result") {
            Some(result) if result.is_object() => Ok(result.clone()),
            _ => Err(HaCliError::new(
                ErrorType::HaApi,
                format!("MCP response for {method} has no result"),
            )),
        }
    }

    /// Перенос `Client._notify`.
    pub fn notify(&mut self, method: &str, params: Option<&Json>) -> Result<(), HaCliError> {
        let payload = build_notification(method, params);
        self.post(&payload)?;
        Ok(())
    }

    /// Перенос `Client.initialize`.
    pub fn initialize(&mut self) -> Result<Json, HaCliError> {
        if self.initialized {
            return Ok(json!({}));
        }
        let result = self.call(
            "initialize",
            Some(&json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ha-cli", "version": "0.1.0"},
            })),
        )?;
        if let Some(pv) = result.get("protocolVersion").and_then(Json::as_str) {
            if !pv.is_empty() {
                self.protocol_version = Some(pv.to_string());
            }
        }
        self.notify("notifications/initialized", None)?;
        self.initialized = true;
        Ok(result)
    }

    /// Перенос `Client.tools_list`.
    pub fn tools_list(&mut self) -> Result<Vec<Json>, HaCliError> {
        self.initialize()?;
        let result = self.call("tools/list", None)?;
        Ok(result
            .get("tools")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Перенос `Client.tools_call`.
    pub fn tools_call(&mut self, name: &str, arguments: &Json) -> Result<Json, HaCliError> {
        self.initialize()?;
        self.call(
            "tools/call",
            Some(&json!({"name": name, "arguments": arguments})),
        )
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

/// Реальный HTTP-транспорт: reqwest blocking + rustls.
pub struct HttpTransport {
    http: reqwest::blocking::Client,
}

impl HttpTransport {
    pub fn new(config: &Config) -> Result<Self, HaCliError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json, text/event-stream"),
        );
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout))
            .default_headers(headers)
            .build()
            .map_err(|e| HaCliError::new(ErrorType::Configuration, e.to_string()))?;
        Ok(Self { http })
    }
}

/// Авторизация запроса:
/// - Assist endpoint (`mcp_url` не задан) — Bearer `HA_TOKEN` как раньше;
/// - `HA_MCP_URL` без `mcp_auth=ha_auth` — токен не отправляется,
///   секретный URL авторизуется сам по себе;
/// - `HA_MCP_URL` с явным `mcp_auth=ha_auth` — Bearer `HA_TOKEN`.
pub fn auth_header(config: &Config) -> Option<String> {
    if config.token.is_empty() {
        return None;
    }
    match config.mcp_url {
        Some(_) => match config.mcp_auth {
            McpAuth::HaAuth => Some(format!("Bearer {}", config.token)),
            McpAuth::None => None,
        },
        None => Some(format!("Bearer {}", config.token)),
    }
}

impl Transport for HttpTransport {
    fn post(
        &mut self,
        url: &str,
        payload: &Json,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, HaCliError> {
        // Endpoint уже разрешён вызывающей стороной (resolve_endpoint):
        // настроенный mcp_url используется как есть, без добавления пути Assist.
        let mut request = self.http.post(url).json(payload);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().map_err(|e| {
            let message = if e.is_timeout() {
                "Request to Home Assistant timed out"
            } else {
                "Unable to connect to Home Assistant"
            };
            HaCliError::new(ErrorType::Connection, message)
        })?;
        let status = response.status().as_u16();
        let resp_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = response.text().map_err(|e| {
            let message = if e.is_timeout() {
                "Request to Home Assistant timed out"
            } else {
                "Unable to connect to Home Assistant"
            };
            HaCliError::new(ErrorType::Connection, message)
        })?;
        Ok(HttpResponse {
            status,
            headers: resp_headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::Secrets;
    use std::cell::RefCell;

    type RecordedRequest = (String, Json, Vec<(String, String)>);

    struct RecordingTransport {
        requests: RefCell<Vec<RecordedRequest>>,
        response: HttpResponse,
    }

    impl RecordingTransport {
        fn new() -> Self {
            Self {
                requests: RefCell::new(Vec::new()),
                response: HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body: json!({"jsonrpc": "2.0", "id": 1, "result": {}}).to_string(),
                },
            }
        }

        fn authorization(&self) -> Option<String> {
            self.requests.borrow().first().and_then(|(_, _, headers)| {
                headers
                    .iter()
                    .find(|(k, _)| k == "Authorization")
                    .map(|(_, v)| v.clone())
            })
        }

        fn last_url(&self) -> String {
            self.requests.borrow().first().unwrap().0.clone()
        }
    }

    impl Transport for RecordingTransport {
        fn post(
            &mut self,
            url: &str,
            payload: &Json,
            headers: &[(String, String)],
        ) -> Result<HttpResponse, HaCliError> {
            self.requests
                .borrow_mut()
                .push((url.to_string(), payload.clone(), headers.to_vec()));
            Ok(HttpResponse {
                status: self.response.status,
                headers: self.response.headers.clone(),
                body: self.response.body.clone(),
            })
        }
    }

    /// Обёртка, позволяющая инспектировать запросы после передачи транспорта
    /// в Client (Box<dyn Transport> забирает владение).
    #[derive(Clone)]
    struct SharedTransport(std::rc::Rc<std::cell::RefCell<RecordingTransport>>);

    impl SharedTransport {
        fn new() -> Self {
            Self(std::rc::Rc::new(std::cell::RefCell::new(
                RecordingTransport::new(),
            )))
        }

        fn authorization(&self) -> Option<String> {
            self.0.borrow().authorization()
        }

        fn last_url(&self) -> String {
            self.0.borrow().last_url()
        }
    }

    impl Transport for SharedTransport {
        fn post(
            &mut self,
            url: &str,
            payload: &Json,
            headers: &[(String, String)],
        ) -> Result<HttpResponse, HaCliError> {
            self.0.borrow_mut().post(url, payload, headers)
        }
    }

    fn config_from(mcp_url: Option<&str>, mcp_auth: McpAuth, token: &str) -> Config {
        Config {
            url: Some("http://ha.local".to_string()),
            mcp_url: mcp_url.map(str::to_string),
            mcp_auth,
            token: token.to_string(),
            timeout: 5,
        }
    }

    #[test]
    fn mcp_url_default_sends_no_bearer() {
        let config = config_from(
            Some("http://ha.local/api/webhook/sec"),
            McpAuth::None,
            "tok",
        );
        assert_eq!(auth_header(&config), None);
    }

    #[test]
    fn mcp_url_ha_auth_sends_bearer() {
        let config = config_from(
            Some("http://ha.local/api/webhook/sec"),
            McpAuth::HaAuth,
            "tok",
        );
        assert_eq!(auth_header(&config), Some("Bearer tok".to_string()));
    }

    #[test]
    fn assist_endpoint_keeps_bearer() {
        let config = config_from(None, McpAuth::None, "tok");
        assert_eq!(auth_header(&config), Some("Bearer tok".to_string()));
    }

    #[test]
    fn empty_token_never_authorizes() {
        let config = config_from(None, McpAuth::None, "");
        assert_eq!(auth_header(&config), None);
    }

    #[test]
    fn request_to_mcp_url_has_no_authorization_by_default() {
        let config = config_from(
            Some("http://ha.local/api/webhook/sec"),
            McpAuth::None,
            "tok",
        );
        let transport = SharedTransport::new();
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        client.initialize().unwrap();
        assert_eq!(transport.authorization(), None);
        assert_eq!(transport.last_url(), "http://ha.local/api/webhook/sec");
    }

    #[test]
    fn request_to_mcp_url_with_ha_auth_sends_authorization() {
        let config = config_from(
            Some("http://ha.local/api/webhook/sec"),
            McpAuth::HaAuth,
            "tok",
        );
        let transport = SharedTransport::new();
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        client.initialize().unwrap();
        assert_eq!(transport.authorization(), Some("Bearer tok".to_string()));
    }

    #[test]
    fn assist_request_keeps_authorization_and_endpoint() {
        let config = config_from(None, McpAuth::None, "tok");
        let transport = SharedTransport::new();
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        client.initialize().unwrap();
        assert_eq!(transport.authorization(), Some("Bearer tok".to_string()));
        assert_eq!(transport.last_url(), "http://ha.local/api/mcp/assist");
    }

    #[test]
    fn mcp_url_is_not_suffixed_with_assist_path() {
        let config = config_from(Some("http://ha.local/private_secret"), McpAuth::None, "tok");
        let transport = SharedTransport::new();
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        client.initialize().unwrap();
        assert_eq!(transport.last_url(), "http://ha.local/private_secret");
    }
}
