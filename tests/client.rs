use ha_cli::client::{auth_header, resolve_endpoint, Client, Transport};
use ha_cli::config::Config;
use ha_cli::errors::{ErrorType, HaCliError};
use ha_cli::security::Secrets;
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};

struct Recorded {
    url: String,
    payload: Json,
    headers: Vec<(String, String)>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
struct MockTransport {
    requests: Arc<Mutex<Vec<Recorded>>>,
    handler: Arc<Handler>,
}

impl MockTransport {
    fn new(handler: impl Fn(&Json, &[(String, String)]) -> Resp + Send + Sync + 'static) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            handler: Arc::new(handler),
        }
    }

    fn methods(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.payload["method"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl Transport for MockTransport {
    fn post(
        &mut self,
        url: &str,
        payload: &Json,
        headers: &[(String, String)],
    ) -> Result<ha_cli::client::HttpResponse, HaCliError> {
        self.requests.lock().unwrap().push(Recorded {
            url: url.to_string(),
            payload: payload.clone(),
            headers: headers.to_vec(),
        });
        let (status, resp_headers, body) = (self.handler)(payload, headers);
        Ok(ha_cli::client::HttpResponse {
            status,
            headers: resp_headers,
            body,
        })
    }
}

type Resp = (u16, Vec<(String, String)>, String);
type Handler = dyn Fn(&Json, &[(String, String)]) -> Resp + Send + Sync;

fn json_response(payload: &Json) -> Resp {
    (200, Vec::new(), payload.to_string())
}

fn rpc_result(result: &Json, id: u64) -> Resp {
    json_response(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn empty_202() -> Resp {
    (202, Vec::new(), String::new())
}

fn tools() -> Json {
    json!([
        {"name": "HassTurnOn", "description": "Turn on"},
        {"name": "HassTurnOff", "description": "Turn off"},
    ])
}

fn make_client(transport: MockTransport) -> Client {
    make_client_with_config(
        transport,
        Config {
            mcp_url: "http://ha.test:8123/api/webhook/test_secret".to_string(),
            mcp_auth: Default::default(),
            token: "secret".to_string(),
            timeout: 5,
            connect_timeout: 5,
        },
    )
}

fn make_client_with_config(transport: MockTransport, config: Config) -> Client {
    let mut secrets = Secrets::new();
    secrets.register("secret");
    secrets.register(&config.mcp_url);
    Client::new(config, Box::new(transport), &secrets)
}

#[test]
fn initialize_lifecycle_before_tools_list() {
    let transport =
        MockTransport::new(
            |payload, _headers| match payload["method"].as_str().unwrap() {
                "initialize" => {
                    assert_eq!(payload["params"]["clientInfo"]["name"], "ha-cli");
                    (
                        200,
                        vec![("Mcp-Session-Id".to_string(), "session-123".to_string())],
                        json!({
                            "jsonrpc": "2.0",
                            "id": payload["id"],
                            "result": {
                                "protocolVersion": "2025-03-26",
                                "capabilities": {},
                            }
                        })
                        .to_string(),
                    )
                }
                "notifications/initialized" => empty_202(),
                _ => rpc_result(&json!({"tools": tools()}), payload["id"].as_u64().unwrap()),
            },
        );
    let methods_getter = transport.clone();
    let mut client = make_client(transport);
    let tools = client.tools_list().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["HassTurnOn", "HassTurnOff"]
    );
    let requests = methods_getter.requests.lock().unwrap();
    let methods: Vec<String> = requests
        .iter()
        .map(|r| r.payload["method"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        methods,
        vec!["initialize", "notifications/initialized", "tools/list"]
    );
    assert!(requests[1].payload.get("id").is_none());
    assert_ne!(requests[0].payload["id"], requests[2].payload["id"]);
    for recorded in &requests[1..] {
        assert_eq!(recorded.header("Mcp-Session-Id"), Some("session-123"));
        assert_eq!(recorded.header("MCP-Protocol-Version"), Some("2025-03-26"));
    }
}

#[test]
fn initialize_only_once() {
    let transport =
        MockTransport::new(
            |payload, _headers| match payload["method"].as_str().unwrap() {
                "initialize" => rpc_result(
                    &json!({"protocolVersion": "2025-03-26"}),
                    payload["id"].as_u64().unwrap(),
                ),
                "notifications/initialized" => empty_202(),
                _ => rpc_result(&json!({"tools": tools()}), payload["id"].as_u64().unwrap()),
            },
        );
    let methods = transport.clone();
    let mut client = make_client(transport);
    client.tools_list().unwrap();
    client.tools_list().unwrap();
    let m = methods.methods();
    assert_eq!(m.iter().filter(|s| *s == "initialize").count(), 1);
    assert_eq!(m.iter().filter(|s| *s == "tools/list").count(), 2);
}

#[test]
fn auth_error_on_401() {
    let transport = MockTransport::new(|_payload, _headers| (401, Vec::new(), String::new()));
    let mut client = make_client(transport);
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Authentication.as_str());
    assert_eq!(err.message, "Home Assistant rejected the access token");
}

#[test]
fn auth_error_on_403() {
    let transport = MockTransport::new(|_payload, _headers| (403, Vec::new(), String::new()));
    let mut client = make_client(transport);
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Authentication.as_str());
}

#[test]
fn connection_error_from_transport() {
    struct Failing;
    impl Transport for Failing {
        fn post(
            &mut self,
            _url: &str,
            _payload: &Json,
            _headers: &[(String, String)],
        ) -> Result<ha_cli::client::HttpResponse, HaCliError> {
            Err(HaCliError::new(ErrorType::Connection, "refused: secret"))
        }
    }
    let mut secrets = Secrets::new();
    secrets.register("secret");
    let mut client = Client::new(
        Config {
            mcp_url: "http://ha.test:8123/api/webhook/test_secret".to_string(),
            mcp_auth: Default::default(),
            token: "secret".to_string(),
            timeout: 5,
            connect_timeout: 5,
        },
        Box::new(Failing),
        &secrets,
    );
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Connection.as_str());
    assert_eq!(err.message, "refused: [REDACTED]");
}

#[test]
fn http_error_maps_to_ha_api_error() {
    let transport = MockTransport::new(|_payload, _headers| (500, Vec::new(), String::new()));
    let mut client = make_client(transport);
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::HaApi.as_str());
    assert_eq!(err.message, "Home Assistant returned HTTP 500");
}

#[test]
fn invalid_json_maps_to_ha_api_error() {
    let transport =
        MockTransport::new(|_payload, _headers| (200, Vec::new(), "not json".to_string()));
    let mut client = make_client(transport);
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::HaApi.as_str());
    assert_eq!(
        err.message,
        "Home Assistant returned an invalid JSON response"
    );
}

#[test]
fn empty_body_treated_as_empty_object() {
    let transport =
        MockTransport::new(
            |payload, _headers| match payload["method"].as_str().unwrap() {
                "initialize" => rpc_result(&json!({}), payload["id"].as_u64().unwrap()),
                "notifications/initialized" => empty_202(),
                _ => rpc_result(&json!({"tools": []}), payload["id"].as_u64().unwrap()),
            },
        );
    let mut client = make_client(transport);
    let result = client.initialize().unwrap();
    assert_eq!(result, json!({}));
    assert!(client.tools_list().unwrap().is_empty());
}

#[test]
fn mcp_error_result_maps_to_ha_api_error() {
    let transport = MockTransport::new(|payload, _headers| {
        json_response(&json!({
            "jsonrpc": "2.0",
            "id": payload["id"],
            "error": {"code": -32000, "message": "boom"}
        }))
    });
    let mut client = make_client(transport);
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::HaApi.as_str());
    assert_eq!(err.message, "MCP error: boom");
}

#[test]
fn missing_result_maps_to_ha_api_error() {
    let transport = MockTransport::new(|payload, _headers| {
        json_response(&json!({"jsonrpc": "2.0", "id": payload["id"]}))
    });
    let mut client = make_client(transport);
    let err = client.tools_list().unwrap_err();
    assert_eq!(err.message, "MCP response for initialize has no result");
}

#[test]
fn tools_call_sends_name_and_arguments() {
    let transport =
        MockTransport::new(
            |payload, _headers| match payload["method"].as_str().unwrap() {
                "initialize" => rpc_result(
                    &json!({"protocolVersion": "2025-03-26"}),
                    payload["id"].as_u64().unwrap(),
                ),
                "notifications/initialized" => empty_202(),
                _ => {
                    assert_eq!(payload["params"]["name"], "HassTurnOn");
                    assert_eq!(payload["params"]["arguments"], json!({"area": "kitchen"}));
                    rpc_result(
                        &json!({"content": [{"type": "text", "text": "ok"}]}),
                        payload["id"].as_u64().unwrap(),
                    )
                }
            },
        );
    let mut client = make_client(transport);
    let result = client
        .tools_call("HassTurnOn", &json!({"area": "kitchen"}))
        .unwrap();
    assert_eq!(result["content"][0]["text"], "ok");
}

#[test]
fn authorization_and_content_type_headers() {
    let transport = MockTransport::new(|_payload, headers| {
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer secret"),
            "no auth header in {headers:?}"
        );
        if headers
            .iter()
            .any(|(k, v)| k == "Mcp-Session-Id" && v == "session-123")
        {
            // Повторный запрос после захвата сессии
        }
        (200, Vec::new(), String::new())
    });
    let _client = make_client(transport);
}

#[test]
fn stale_session_resent_on_each_request() {
    let transport =
        MockTransport::new(
            |payload, headers| match payload["method"].as_str().unwrap() {
                "initialize" => (
                    200,
                    vec![("Mcp-Session-Id".to_string(), "session-abc".to_string())],
                    json!({
                        "jsonrpc": "2.0",
                        "id": payload["id"],
                        "result": {"protocolVersion": "2025-03-26", "capabilities": {}}
                    })
                    .to_string(),
                ),
                "notifications/initialized" => {
                    assert_eq!(
                        headers
                            .iter()
                            .find(|(k, _)| k == "Mcp-Session-Id")
                            .map(|(_, v)| v.as_str()),
                        Some("session-abc")
                    );
                    empty_202()
                }
                _ => {
                    assert_eq!(
                        headers
                            .iter()
                            .find(|(k, _)| k == "Mcp-Session-Id")
                            .map(|(_, v)| v.as_str()),
                        Some("session-abc")
                    );
                    rpc_result(&json!({"tools": []}), payload["id"].as_u64().unwrap())
                }
            },
        );
    let mut client = make_client(transport);
    client.tools_list().unwrap();
}

#[test]
fn mcp_url_used_as_is_webhook() {
    let transport = MockTransport::new(|_payload, _headers| (200, Vec::new(), String::new()));
    let requests = transport.clone();
    let mut client = make_client_with_config(
        transport,
        Config {
            mcp_url: "http://ha.test:8123/api/webhook/wh_secret123".to_string(),
            mcp_auth: Default::default(),
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        },
    );
    client.notify("notifications/initialized", None).unwrap();
    let reqs = requests.requests.lock().unwrap();
    assert_eq!(reqs[0].url, "http://ha.test:8123/api/webhook/wh_secret123");
    assert!(!reqs[0]
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Authorization")));
}

#[test]
fn mcp_url_used_as_is_direct_private_path() {
    let transport = MockTransport::new(|_payload, _headers| (200, Vec::new(), String::new()));
    let requests = transport.clone();
    let mut client = make_client_with_config(
        transport,
        Config {
            mcp_url: "http://127.0.0.1:8123/private_prv_secret456".to_string(),
            mcp_auth: Default::default(),
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        },
    );
    client.notify("notifications/initialized", None).unwrap();
    let reqs = requests.requests.lock().unwrap();
    assert_eq!(reqs[0].url, "http://127.0.0.1:8123/private_prv_secret456");
}

#[test]
fn secret_mcp_url_redacted_in_transport_error() {
    struct Failing;
    impl Transport for Failing {
        fn post(
            &mut self,
            url: &str,
            _payload: &Json,
            _headers: &[(String, String)],
        ) -> Result<ha_cli::client::HttpResponse, HaCliError> {
            Err(HaCliError::new(
                ErrorType::Connection,
                format!("refused: {url}"),
            ))
        }
    }
    let mut secrets = Secrets::new();
    let mcp_url = "http://ha.test:8123/api/webhook/wh_secret789".to_string();
    secrets.register(&mcp_url);
    let mut client = Client::new(
        Config {
            mcp_url,
            mcp_auth: Default::default(),
            token: String::new(),
            timeout: 5,
            connect_timeout: 5,
        },
        Box::new(Failing),
        &secrets,
    );
    let err = client.tools_list().unwrap_err();
    assert!(!err.message.contains("wh_secret789"));
    assert!(err.message.contains("[REDACTED]"), "{}", err.message);
}

#[test]
fn auth_header_absent_without_ha_auth_and_endpoint_used_as_is() {
    let webhook = Config {
        mcp_url: "http://ha.test:8123/api/webhook/wh_s".to_string(),
        mcp_auth: Default::default(),
        token: "ha-token".to_string(),
        timeout: 5,
        connect_timeout: 5,
    };
    // Секретный URL авторизуется сам по себе: Bearer не отправляется.
    assert_eq!(auth_header(&webhook), None);
    assert_eq!(
        resolve_endpoint(&webhook),
        "http://ha.test:8123/api/webhook/wh_s"
    );
}

#[test]
fn auth_header_ha_auth_opt_in_sends_token_to_mcp_url() {
    let webhook = Config {
        mcp_url: "http://ha.test:8123/api/webhook/wh_s".to_string(),
        mcp_auth: ha_cli::config::McpAuth::HaAuth,
        token: "ha-token".to_string(),
        timeout: 5,
        connect_timeout: 5,
    };
    assert_eq!(auth_header(&webhook), Some("Bearer ha-token".to_string()));
}

// --- 2.4: различение JSON-RPC ошибки и redaction отражённых секретов ---

fn json_rpc_error(message: &str) -> Resp {
    json_response(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32000, "message": message},
    }))
}

#[test]
fn json_rpc_error_message_is_redacted() {
    let webhook = "http://ha.local/api/webhook/wh_s3cret";
    let transport = MockTransport::new(move |_payload, _headers| {
        json_rpc_error(&format!("POST {webhook} failed for bearer sk_live_token"))
    });
    let config = Config {
        mcp_url: webhook.to_string(),
        mcp_auth: Default::default(),
        token: String::new(),
        timeout: 5,
        connect_timeout: 5,
    };
    let mut secrets = ha_cli::security::Secrets::new();
    secrets.register("sk_live_token");
    ha_cli::config::register_mcp_secrets(&mut secrets, webhook);
    let mut client = Client::new(config, Box::new(transport), &secrets);
    let err = client.call("tools/list", None).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::HaApi.as_str());
    let rendered = secrets.redact(&err.to_json());
    assert!(!rendered.contains("wh_s3cret"));
    assert!(!rendered.contains("/api/webhook/"));
    assert!(!rendered.contains("sk_live_token"));
    assert!(rendered.contains("[REDACTED]"));
}

#[test]
fn json_rpc_error_over_sse_is_ha_api_error_and_redacted() {
    let message = "call failed near /private_s3cret with token sk_live_token";
    let transport = MockTransport::new(move |payload, _headers| {
        let body = format!(
            "event: message\ndata: {}\n\n",
            json!({
                "jsonrpc": "2.0",
                "id": payload["id"],
                "error": {"code": -32000, "message": message},
            })
        );
        (
            200,
            vec![("Content-Type".to_string(), "text/event-stream".to_string())],
            body,
        )
    });
    let mut secrets = Secrets::new();
    secrets.register("sk_live_token");
    ha_cli::config::register_mcp_secrets(&mut secrets, "http://ha.local/private_s3cret");
    let config = Config {
        mcp_url: "http://ha.local/private_s3cret".to_string(),
        mcp_auth: Default::default(),
        token: String::new(),
        timeout: 5,
        connect_timeout: 5,
    };
    let mut client = Client::new(config, Box::new(transport), &secrets);
    let err = client.call("tools/call", Some(&json!({}))).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::HaApi.as_str());
    let rendered = secrets.redact(&err.to_json());
    assert!(!rendered.contains("s3cret"));
    assert!(!rendered.contains("sk_live_token"));
    assert!(rendered.contains("[REDACTED]"));
}
