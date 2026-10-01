use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::errors::{ErrorType, HaCliError};
use ha_cli::intents::{execute, validate_intent, INITIAL_INTENT_SET};
use ha_cli::security::Secrets;
use serde_json::{json, Value as Json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

// ---------- Изоляция кэша (аналог fixture isolated_cache) ----------

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct IsolatedCache {
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

fn isolated_cache() -> IsolatedCache {
    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path());
    IsolatedCache {
        _guard: guard,
        _dir: dir,
    }
}

// ---------- Mock-транспорт (аналог FakeClient из test_intents.py) ----------

#[derive(Clone)]
struct MockTransport {
    /// Последовательность ответов tools/list (последний повторяется).
    tools: Arc<Mutex<VecDeque<Vec<Json>>>>,
    call_results: Arc<Mutex<VecDeque<(u16, Json)>>>,
    tools_list_calls: Arc<Mutex<u32>>,
    tool_calls: Arc<Mutex<Vec<(String, Json)>>>,
}

impl MockTransport {
    fn new(tools: Vec<Json>) -> Self {
        Self {
            tools: Arc::new(Mutex::new(VecDeque::from(vec![tools]))),
            call_results: Arc::new(Mutex::new(VecDeque::new())),
            tools_list_calls: Arc::new(Mutex::new(0)),
            tool_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_call_result(&self, status: u16, result: Json) {
        self.call_results
            .lock()
            .unwrap()
            .push_back((status, result));
    }

    fn tool_calls(&self) -> Vec<(String, Json)> {
        self.tool_calls.lock().unwrap().clone()
    }
}

fn rpc_result(result: &Json, id: &Json) -> Resp {
    (
        200,
        Vec::new(),
        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    )
}

type Resp = (u16, Vec<(String, String)>, String);

impl Transport for MockTransport {
    fn post(
        &mut self,
        _url: &str,
        payload: &Json,
        _headers: &[(String, String)],
    ) -> Result<HttpResponse, HaCliError> {
        match payload["method"].as_str().unwrap_or("") {
            "initialize" => {
                let (status, headers, body) =
                    rpc_result(&json!({"protocolVersion": "2025-03-26"}), &payload["id"]);
                Ok(http_response(status, headers, body))
            }
            "notifications/initialized" => Ok(http_response(202, Vec::new(), String::new())),
            "tools/list" => {
                *self.tools_list_calls.lock().unwrap() += 1;
                let mut queue = self.tools.lock().unwrap();
                let tools = match queue.pop_front() {
                    Some(tools) => {
                        queue.push_back(tools.clone());
                        tools
                    }
                    None => queue.back().cloned().unwrap_or_default(),
                };
                drop(queue);
                let (status, headers, body) = rpc_result(&json!({"tools": tools}), &payload["id"]);
                Ok(http_response(status, headers, body))
            }
            "tools/call" => {
                let name = payload["params"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let arguments = payload["params"]["arguments"].clone();
                self.tool_calls.lock().unwrap().push((name, arguments));
                let (call_status, result) = self
                    .call_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((200, default_call_result()));
                let (status, headers, body) = rpc_result(&result, &payload["id"]);
                Ok(http_response(
                    if call_status == 200 {
                        status
                    } else {
                        call_status
                    },
                    headers,
                    body,
                ))
            }
            _ => {
                let (status, headers, body) = rpc_result(&json!({}), &payload["id"]);
                Ok(http_response(status, headers, body))
            }
        }
    }
}

fn http_response(status: u16, headers: Vec<(String, String)>, body: String) -> HttpResponse {
    HttpResponse {
        status,
        headers,
        body,
    }
}

/// Инструменты ha-mcp: путь интентов всегда идёт через каталог
/// (ha_get_overview → ha_search → ha_get_state / ha_call_service).
fn hamcp_tools() -> Vec<Json> {
    vec![
        json!({"name": "ha_get_overview"}),
        json!({"name": "ha_search"}),
        json!({"name": "ha_get_state"}),
        json!({"name": "ha_call_service"}),
    ]
}

fn default_call_result() -> Json {
    json!({"content": [{"type": "text", "text": "Turned on"}], "isError": false})
}

fn make_client(transport: &MockTransport) -> Client {
    let secrets = Secrets::new();
    let config = Config {
        mcp_url: "http://ha.test/api/webhook/testsecret123".to_string(),
        mcp_auth: Default::default(),
        token: String::new(),
        timeout: 5,
        connect_timeout: 5,
    };
    Client::new(config, Box::new(transport.clone()), &secrets)
}

// --- validate_intent / allowlist ---

#[test]
fn initial_intent_set_is_fixed() {
    assert_eq!(
        INITIAL_INTENT_SET,
        &[
            "HassTurnOn",
            "HassTurnOff",
            "HassGetState",
            "HassLightSet",
            "HassSetPosition",
        ]
    );
}

#[test]
fn validate_intent_accepts_allowed() {
    for intent in INITIAL_INTENT_SET {
        validate_intent(intent).unwrap();
    }
}

#[test]
fn validate_intent_rejects_unknown() {
    let err = validate_intent("HassNuke").unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::InvalidArguments.as_str());
}

#[test]
fn validate_intent_rejects_similar() {
    assert!(validate_intent("HassTurnOnExtra").is_err());
}

#[test]
fn blocked_intent_is_exit_code_2() {
    let err = validate_intent("HassBroadcast").unwrap_err();
    assert_eq!(err.kind.exit_code(), 2);
}

// --- execute ---

// --- security: запрет entity_id в payload интентов (из test_security.py) ---

#[test]
fn entity_id_key_top_level_rejected() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    let mut client = make_client(&transport);

    let err = execute(
        &mut client,
        "HassTurnOn",
        &json!({"entity_id": "light.kitchen"}),
    )
    .unwrap_err();

    assert_eq!(err.kind.exit_code(), 2);
    assert_eq!(transport.tool_calls().len(), 0);
}

#[test]
fn entity_id_variant_keys_rejected() {
    for key in ["entityid", "ENTITY-ID", "entities_ids", "target_entity_id"] {
        let _cache = isolated_cache();
        let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
        let mut client = make_client(&transport);

        let err = execute(&mut client, "HassTurnOn", &json!({key: "light.kitchen"})).unwrap_err();

        assert_eq!(err.kind.exit_code(), 2, "{key}");
        assert_eq!(transport.tool_calls().len(), 0, "{key}");
    }
}

#[test]
fn entity_id_key_nested_rejected() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    let mut client = make_client(&transport);

    let err = execute(
        &mut client,
        "HassTurnOn",
        &json!({"area": "Kitchen", "options": {"targets": [{"entity_id": "light.kitchen"}]}}),
    )
    .unwrap_err();

    assert_eq!(err.kind.exit_code(), 2);
    assert_eq!(transport.tool_calls().len(), 0);
}

#[test]
fn entity_id_list_nested_rejected() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    let mut client = make_client(&transport);

    let err = execute(
        &mut client,
        "HassTurnOn",
        &json!({"area": "Kitchen", "entity_ids": ["light.a", "switch.b"]}),
    )
    .unwrap_err();

    assert_eq!(err.kind.exit_code(), 2);
    assert_eq!(transport.tool_calls().len(), 0);
}

#[test]
fn entity_id_like_value_rejected() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    let mut client = make_client(&transport);

    let err = execute(
        &mut client,
        "HassTurnOn",
        &json!({"area": "Kitchen", "name": "light.kitchen"}),
    )
    .unwrap_err();

    assert_eq!(err.kind.exit_code(), 2);
    assert_eq!(transport.tool_calls().len(), 0);
}

#[test]
fn blocked_intent_names_cannot_reach_mcp() {
    for name in ["tools/call", "initialize", "HassBroadcast", "service.call"] {
        let _cache = isolated_cache();
        let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
        let mut client = make_client(&transport);

        let err = execute(&mut client, name, &json!({})).unwrap_err();

        assert_eq!(err.kind.exit_code(), 2, "{name}");
        assert_eq!(transport.tool_calls().len(), 0, "{name}");
    }
}

#[test]
fn tools_call_http_401_is_authentication_error_without_retry() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(hamcp_tools());
    transport.push_call_result(401, default_call_result());
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassTurnOn", &json!({"area": "Kitchen"})).unwrap_err();
    eprintln!("ERR={err:?}");
    assert_eq!(err.kind.exit_code(), 4);
    assert_eq!(transport.tool_calls().len(), 1);
}

#[test]
fn tools_call_http_403_is_authentication_error_without_retry() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(hamcp_tools());
    transport.push_call_result(403, default_call_result());
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassTurnOn", &json!({"area": "Kitchen"})).unwrap_err();

    assert_eq!(err.kind.exit_code(), 4);
    assert_eq!(transport.tool_calls().len(), 1);
}

#[test]
fn tools_call_is_error_result_is_intent_error_without_retry() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(hamcp_tools());
    transport.push_call_result(
        200,
        json!({
            "content": [{"type": "text", "text": "Tool execution failed"}],
            "isError": true,
        }),
    );
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassTurnOn", &json!({"area": "Kitchen"})).unwrap_err();

    // Первый tools/call — ha_get_overview: isError инструмента каталога —
    // context-ошибка; повторного вызова tools/call нет.
    assert_eq!(err.kind.exit_code(), 10);
    assert_eq!(transport.tool_calls().len(), 1);
}
