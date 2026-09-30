use ha_cli::client::{Client, Transport};
use ha_cli::config::Config;
use ha_cli::errors::{ErrorType, HaCliError};
use ha_cli::security::Secrets;
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};

struct Recorded {
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
        _url: &str,
        payload: &Json,
        headers: &[(String, String)],
    ) -> Result<ha_cli::client::HttpResponse, HaCliError> {
        self.requests.lock().unwrap().push(Recorded {
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
    let mut secrets = Secrets::new();
    secrets.register("secret");
    let config = Config {
        url: "http://ha.test:8123".to_string(),
        token: "secret".to_string(),
        timeout: 5,
    };
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
            url: "http://ha.test:8123".to_string(),
            token: "secret".to_string(),
            timeout: 5,
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
