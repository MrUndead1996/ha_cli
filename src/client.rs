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

    fn parse_body(&self, response: &HttpResponse, expected_id: u64) -> Result<Json, HaCliError> {
        if response.body.trim().is_empty() {
            // Ответ на notification (например, HTTP 202) не содержит тела.
            return Ok(json!({}));
        }
        let content_type = response.header("Content-Type").unwrap_or_default();
        let data = if content_type.contains("text/event-stream") {
            // ha-mcp отвечает SSE: искать JSON-RPC сообщение с нашим id.
            match parse_sse_message(&response.body, expected_id)? {
                Some(message) => message,
                None => {
                    return Err(HaCliError::new(
                        ErrorType::HaApi,
                        "MCP event-stream response does not contain a matching JSON-RPC reply",
                    ))
                }
            }
        } else {
            serde_json::from_str(&response.body).map_err(|_| {
                HaCliError::new(
                    ErrorType::HaApi,
                    "Home Assistant returned an invalid JSON response",
                )
            })?
        };
        // Ответ обязан соответствовать отправленному запросу по JSON-RPC id.
        if data.get("id").and_then(Json::as_u64) != Some(expected_id) {
            return Err(HaCliError::new(
                ErrorType::HaApi,
                format!("MCP response for {expected_id} has a mismatched JSON-RPC id"),
            ));
        }
        Ok(data)
    }

    /// Перенос `Client._call`.
    pub fn call(&mut self, method: &str, params: Option<&Json>) -> Result<Json, HaCliError> {
        self.next_id += 1;
        let payload = build_request(self.next_id, method, params);
        let response = self.post(&payload)?;
        let data = self.parse_body(&response, self.next_id)?;
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

/// Ограничение числа SSE-событий в одном ответе: защищает от бесконечного
/// потока событий без соответствующего id.
pub const MAX_SSE_EVENTS: usize = 10_000;

/// Разбор SSE-тела: возвращает JSON-RPC сообщение, чей `id` совпадает с
/// ожидаемым. `Ok(None)` — совпадения нет. События без `data`, комментарии
/// и не-JSON данные пропускаются; число событий ограничено `MAX_SSE_EVENTS`.
pub fn parse_sse_message(body: &str, expected_id: u64) -> Result<Option<Json>, HaCliError> {
    let mut data = String::new();
    let mut events = 0usize;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        } else if !line.trim().is_empty() {
            // event:/id:/retry:/комментарии игнорируются: важно только data.
            continue;
        } else {
            // Пустая строка завершает событие.
            if let Some(message) = take_event(&mut data, expected_id, &mut events)? {
                return Ok(Some(message));
            }
        }
    }
    take_event(&mut data, expected_id, &mut events)
}

/// Финализирует накопленный `data` одного события. Возвращает `Ok(Some(msg))`,
/// если это JSON с совпадающим `id`; `Ok(None)` — иначе.
fn take_event(
    data: &mut String,
    expected_id: u64,
    events: &mut usize,
) -> Result<Option<Json>, HaCliError> {
    if data.is_empty() {
        return Ok(None);
    }
    *events += 1;
    if *events > MAX_SSE_EVENTS {
        return Err(HaCliError::new(
            ErrorType::HaApi,
            "MCP event-stream response has too many events",
        ));
    }
    let parsed = serde_json::from_str::<Json>(data).ok();
    data.clear();
    match parsed {
        Some(message) if message.get("id").and_then(Json::as_u64) == Some(expected_id) => {
            Ok(Some(message))
        }
        _ => Ok(None),
    }
}

/// Реальный HTTP-транспорт: reqwest blocking + rustls.
pub struct HttpTransport {
    http: reqwest::blocking::Client,
}

/// Верхняя граница размера тела ответа: неограниченное чтение SSE-потока
/// способно исчерпать память.
pub const MAX_RESPONSE_BODY_BYTES: u64 = 10 * 1024 * 1024;

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
            // Таймаут установки соединения отделён от общего таймаута
            // исполнения: сервисы ha-mcp могут ожидать подтверждения
            // изменения состояния внутри уже открытого запроса.
            .connect_timeout(std::time::Duration::from_secs(config.connect_timeout))
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
        // Читаем не больше MAX_RESPONSE_BODY_BYTES + 1 байт: лишний байт
        // означает, что ответ усечён.
        use std::io::Read as _;
        let mut bytes = Vec::new();
        // Запрос уже отправлен: таймаут здесь не означает, что операция не
        // выполнена (сервисы ha-mcp могут ждать подтверждения состояния).
        if let Err(e) = response
            .take(MAX_RESPONSE_BODY_BYTES + 1)
            .read_to_end(&mut bytes)
        {
            // reqwest оборачивает таймаут в io::Error (kind Other) с исходным
            // reqwest::Error внутри — распознаём таймаут через downcast.
            let timed_out = e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
                .map(reqwest::Error::is_timeout)
                .unwrap_or_else(|| e.kind() == std::io::ErrorKind::TimedOut);
            return Err(if timed_out {
                HaCliError::new(
                    ErrorType::Connection,
                    "Home Assistant response timed out after the request was sent; \
                 the operation may still have been performed",
                )
            } else {
                HaCliError::new(
                    ErrorType::Connection,
                    "Unable to read response from Home Assistant",
                )
            });
        }
        if bytes.len() as u64 > MAX_RESPONSE_BODY_BYTES {
            return Err(HaCliError::new(
                ErrorType::HaApi,
                "Home Assistant response exceeds the size limit",
            ));
        }
        let body = String::from_utf8(bytes).map_err(|_| {
            HaCliError::new(
                ErrorType::HaApi,
                "Home Assistant returned a non-UTF-8 response",
            )
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
            connect_timeout: 5,
        }
    }

    fn sse_response(id: u64, result: Json) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: format!(
                "event: message\ndata: {}\n\n",
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            ),
        }
    }

    fn empty_response(status: u16) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: String::new(),
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

    #[test]
    fn sse_initialize_and_tools_list_are_parsed_by_id() {
        let config = config_from(
            Some("http://ha.local/api/webhook/sec"),
            McpAuth::HaAuth,
            "tok",
        );
        let transport = SharedTransport::new();
        transport.0.borrow_mut().response =
            sse_response(1, json!({"protocolVersion": "2025-03-26"}));
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        let init = client.initialize().unwrap();
        assert_eq!(
            init.get("protocolVersion").and_then(Json::as_str),
            Some("2025-03-26")
        );
        transport.0.borrow_mut().response =
            sse_response(2, json!({"tools": [{"name": "ha_search"}]}));
        let tools = client.tools_list().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("name").and_then(Json::as_str),
            Some("ha_search")
        );
        // После initialize последующие запросы несут версию протокола.
        let requests = transport.0.borrow().requests.borrow().clone();
        let last_headers = &requests.last().unwrap().2;
        assert!(last_headers
            .iter()
            .any(|(k, v)| k == "MCP-Protocol-Version" && v == "2025-03-26"));
    }

    #[test]
    fn notification_with_empty_202_body_is_accepted() {
        let config = config_from(None, McpAuth::None, "tok");
        let transport = SharedTransport::new();
        transport.0.borrow_mut().response = empty_response(202);
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        client.notify("notifications/initialized", None).unwrap();
    }

    #[test]
    fn sse_event_without_matching_id_is_an_error() {
        let config = config_from(None, McpAuth::None, "tok");
        let transport = SharedTransport::new();
        transport.0.borrow_mut().response = sse_response(99, json!({}));
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        let err = client.call("tools/list", None).unwrap_err();
        assert!(matches!(err.kind, ErrorType::HaApi));
        assert!(err.message.contains("matching JSON-RPC reply"));
    }

    #[test]
    fn malformed_sse_is_a_sanitized_error() {
        let config = config_from(None, McpAuth::None, "tok");
        let transport = SharedTransport::new();
        transport.0.borrow_mut().response = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body: "data: not-json\n\n".to_string(),
        };
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        let err = client.call("tools/list", None).unwrap_err();
        assert!(matches!(err.kind, ErrorType::HaApi));
        // Тело ответа не отражается в ошибке.
        assert!(!err.message.contains("not-json"));
    }

    #[test]
    fn json_response_with_mismatched_id_is_rejected() {
        let config = config_from(None, McpAuth::None, "tok");
        let transport = SharedTransport::new();
        transport.0.borrow_mut().response = HttpResponse {
            status: 200,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: json!({"jsonrpc": "2.0", "id": 42, "result": {}}).to_string(),
        };
        let mut client = Client::new(config, Box::new(transport.clone()), &Secrets::new());
        let err = client.call("tools/list", None).unwrap_err();
        assert!(err.message.contains("mismatched JSON-RPC id"));
    }

    #[test]
    fn sse_parser_selects_event_by_id_among_noise() {
        let body = ": comment\n\
                    data: {\"jsonrpc\": \"2.0\", \"id\": 1, \"result\": {\"n\": 1}}\n\
                    \n\
                    data: {\"jsonrpc\": \"2.0\", \"method\": \"x\"}\n\
                    \n\
                    data: {\"jsonrpc\": \"2.0\", \"id\": 2,\n\
                    data:  \"result\": {\"tools\": [1, 2]}}\n\
                    \n";
        let message = parse_sse_message(body, 2).unwrap().unwrap();
        assert_eq!(message["result"]["tools"].as_array().unwrap().len(), 2);
        assert!(parse_sse_message(body, 7).unwrap().is_none());
    }

    #[test]
    fn sse_parser_is_bounded() {
        let event = "data: {\"jsonrpc\": \"2.0\", \"id\": 1, \"result\": {}}\n\n";
        let body = event.repeat(MAX_SSE_EVENTS + 1);
        // Ни одно событие не совпадает: парсер ограничен числом событий.
        let err = parse_sse_message(&body, 99).unwrap_err();
        assert!(err.message.contains("too many events"));
    }

    /// Минимальный локальный HTTP/1.1 сервер: отдаёт заготовленные ответы по
    /// одному на соединение — для проверки реального HttpTransport. Возвращает
    /// адрес и головы полученных запросов.
    fn serve_responses(
        responses: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let seen_thread = seen.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for response in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let mut data = Vec::new();
                loop {
                    let n = match sock.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    data.extend_from_slice(&buf[..n]);
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                // Дочитываем тело запроса по Content-Length.
                let head = String::from_utf8_lossy(&data).to_string();
                if let Some(len) = head.split("\r\n").find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                }) {
                    let header_end = head.find("\r\n\r\n").map(|i| i + 4).unwrap_or(data.len());
                    while data.len() < header_end + len {
                        match sock.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                        }
                    }
                }
                seen_thread.lock().unwrap().push(head);
                sock.write_all(response.as_bytes()).ok();
            }
        });
        (format!("http://{addr}/api/webhook/test"), seen)
    }

    /// Собирает сырой HTTP-ответ с корректным Content-Length для SSE-тела.
    fn sse_raw(id: u64, result: &str) -> String {
        let data = format!(
            "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}\n\n"
        );
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nMcp-Session-Id: sess-1\r\nContent-Length: {}\r\n\r\n{}",
            data.len(),
            data
        )
    }

    const ACCEPTED_RESPONSE: &str = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n";

    #[test]
    fn live_transport_reads_sse_and_echoes_session_header() {
        let init = sse_raw(1, "{\"protocolVersion\":\"2025-03-26\"}");
        let tools = sse_raw(2, "{\"tools\":[{\"name\":\"ha_search\"}]}");
        let (url, seen) = serve_responses(vec![init, ACCEPTED_RESPONSE.to_string(), tools]);
        let config = config_from(Some(&url), McpAuth::None, "");
        let transport = HttpTransport::new(&config).unwrap();
        let mut client = Client::new(config, Box::new(transport), &Secrets::new());
        let tools = client.tools_list().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("name").and_then(Json::as_str),
            Some("ha_search")
        );
        // Сессия, полученная от initialize, повторяется в tools/list.
        let heads = seen.lock().unwrap().clone();
        // reqwest нормализует имена заголовков в нижний регистр.
        assert!(heads
            .last()
            .unwrap()
            .to_ascii_lowercase()
            .contains("mcp-session-id: sess-1"));
    }

    #[test]
    fn live_transport_reports_auth_failure() {
        let (url, _seen) = serve_responses(vec![
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string(),
        ]);
        let config = config_from(Some(&url), McpAuth::None, "");
        let transport = HttpTransport::new(&config).unwrap();
        let mut client = Client::new(config, Box::new(transport), &Secrets::new());
        let err = client.call("tools/list", None).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Authentication));
    }

    #[test]
    fn read_timeout_after_sent_request_does_not_claim_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            // Заголовки отправлены сразу, тело — после истечения клиентского
            // таймаута: ошибка возникает уже на чтении ответа.
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
            )
            .ok();
            std::thread::sleep(std::time::Duration::from_secs(2));
            sock.write_all(b"{}").ok();
        });
        let mut config = config_from(Some(&format!("http://{addr}/")), McpAuth::None, "");
        config.timeout = 1;
        config.connect_timeout = 1;
        let transport = HttpTransport::new(&config).unwrap();
        let mut client = Client::new(config, Box::new(transport), &Secrets::new());
        let err = client.call("tools/list", None).unwrap_err();
        assert!(matches!(err.kind, ErrorType::Connection));
        assert!(err.message.contains("request was sent"));
        assert!(err.message.contains("may still have been performed"));
    }
}
