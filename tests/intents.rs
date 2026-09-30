use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::errors::{ErrorType, HaCliError};
use ha_cli::intents::{execute, normalize_result, validate_intent, INITIAL_INTENT_SET};
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

    fn push_tools_list(&self, tools: Vec<Json>) {
        self.tools.lock().unwrap().push_back(tools);
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

    fn tools_list_calls(&self) -> u32 {
        *self.tools_list_calls.lock().unwrap()
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

fn default_call_result() -> Json {
    json!({"content": [{"type": "text", "text": "Turned on"}], "isError": false})
}

fn make_client(transport: &MockTransport) -> Client {
    let secrets = Secrets::new();
    let config = Config {
        url: Some("http://ha.test:8123".to_string()),
        mcp_url: None,
        mcp_auth: Default::default(),
        token: String::new(),
        timeout: 5,
        connect_timeout: 5,
    };
    Client::new(config, Box::new(transport.clone()), &secrets)
}

fn tool_result(text: &str, is_error: bool, structured: Option<Json>) -> Json {
    let mut result = json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
    });
    if let Some(structured) = structured {
        result["structuredContent"] = structured;
    }
    result
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

// --- normalize_result ---

#[test]
fn normalize_success_with_text() {
    let data = normalize_result(&tool_result("Turned on the light", false, None)).unwrap();
    assert_eq!(
        data,
        json!({
            "ok": true,
            "response_type": "action_done",
            "speech": "Turned on the light",
        })
    );
}

#[test]
fn normalize_accepts_direct_call_tool_result_shape() {
    let data = normalize_result(&json!({"content": [], "isError": false})).unwrap();
    assert_eq!(data["ok"], json!(true));
    assert_eq!(data["speech"], json!(""));
}

#[test]
fn normalize_rejects_wrapped_result() {
    let wrapped = json!({"result": {"content": [], "isError": false}});
    let err = normalize_result(&wrapped).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Intent.as_str());
}

#[test]
fn normalize_is_error_raises_intent_error() {
    let err = normalize_result(&tool_result("target not found", true, None)).unwrap_err();
    assert!(err.message.contains("target not found"));
    assert_eq!(err.kind.exit_code(), 7);
}

#[test]
fn normalize_is_error_without_text() {
    let err = normalize_result(&json!({"content": [], "isError": true})).unwrap_err();
    assert_eq!(err.message, "intent execution failed");
}

#[test]
fn normalize_structured_content_included() {
    let data = normalize_result(&tool_result(
        "ok",
        false,
        Some(json!({"entities": ["light.kitchen"]})),
    ))
    .unwrap();
    assert_eq!(data["data"], json!({"entities": ["light.kitchen"]}));
}

#[test]
fn normalize_home_assistant_json_text() {
    let response = json!({
        "speech": {"plain": {"speech": "Включено"}},
        "response_type": "action_done",
        "data": {"success": [{"name": "light_kitchen"}], "failed": []},
    });

    let data = normalize_result(&tool_result(&response.to_string(), false, None)).unwrap();

    assert_eq!(
        data,
        json!({
            "ok": true,
            "response_type": "action_done",
            "speech": "Включено",
            "data": {"success": [{"name": "light_kitchen"}], "failed": []},
        })
    );
}

#[test]
fn normalize_home_assistant_empty_speech() {
    let data = normalize_result(&tool_result(
        r#"{"speech": {}, "response_type": "action_done", "data": {}}"#,
        false,
        None,
    ))
    .unwrap();
    assert_eq!(
        data,
        json!({
            "ok": true,
            "response_type": "action_done",
            "speech": "",
            "data": {},
        })
    );
}

#[test]
fn normalize_ignores_non_text_content() {
    let data = normalize_result(&json!({
        "content": [{"type": "image", "data": "x"}],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(data["speech"], json!(""));
}

#[test]
fn normalize_rejects_missing_content() {
    assert!(normalize_result(&json!({"isError": false})).is_err());
}

#[test]
fn normalize_rejects_malformed_content() {
    assert!(normalize_result(&json!({"content": "not-a-list", "isError": false})).is_err());
}

#[test]
fn normalize_rejects_non_dict() {
    assert!(normalize_result(&json!([1, 2, 3])).is_err());
}

#[test]
fn normalize_key_insertion_order_matches_python() {
    // Python-версия строит dict в порядке ok, response_type, speech, data.
    let data = normalize_result(&tool_result(
        r#"{"response_type": "query_answer", "speech": {"plain": {"speech": "off"}}, "data": {"x": 1}}"#,
        false,
        None,
    ))
    .unwrap();
    let serialized = data.to_string();
    let ok_pos = serialized.find("\"ok\":").unwrap();
    let rt_pos = serialized.find("\"response_type\":").unwrap();
    let speech_pos = serialized.find("\"speech\":").unwrap();
    let data_pos = serialized.find("\"data\":").unwrap();
    assert!(ok_pos < rt_pos && rt_pos < speech_pos && speech_pos < data_pos);
}

// --- execute ---

#[test]
fn execute_success() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![
        json!({"name": "HassTurnOn"}),
        json!({"name": "HassTurnOff"}),
    ]);
    let mut client = make_client(&transport);

    let data = execute(
        &mut client,
        "HassTurnOn",
        &json!({"area": "Кухня", "domain": "light"}),
    )
    .unwrap();

    assert_eq!(data["ok"], json!(true));
    assert_eq!(data["speech"], json!("Turned on"));
    assert_eq!(
        transport.tool_calls(),
        vec![(
            "HassTurnOn".to_string(),
            json!({"area": "Кухня", "domain": "light"}),
        )]
    );
}

#[test]
fn execute_rejects_blocked_intent_without_calling() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassNobody", &json!({})).unwrap_err();

    assert_eq!(err.kind.as_str(), ErrorType::InvalidArguments.as_str());
    assert_eq!(transport.tool_calls().len(), 0);
    assert_eq!(transport.tools_list_calls(), 0);
}

#[test]
fn execute_tool_missing() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOff"})]);
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassTurnOn", &json!({})).unwrap_err();

    assert_eq!(err.kind.as_str(), ErrorType::ToolNotFound.as_str());
    assert_eq!(transport.tool_calls().len(), 0);
}

#[test]
fn execute_ambiguous_tool() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![
        json!({"name": "server1__HassTurnOn"}),
        json!({"name": "server2__HassTurnOn"}),
    ]);
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassTurnOn", &json!({})).unwrap_err();

    assert_eq!(err.kind.as_str(), ErrorType::AmbiguousTool.as_str());
    assert_eq!(transport.tool_calls().len(), 0);
}

#[test]
fn execute_tool_call_runtime_error_propagates() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    transport.push_call_result(500, json!(null));
    let mut client = make_client(&transport);

    let err = execute(&mut client, "HassTurnOn", &json!({"area": "Кухня"})).unwrap_err();

    assert_eq!(err.kind.as_str(), ErrorType::HaApi.as_str());
}

#[test]
fn execute_uses_prefixed_mcp_name() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "assist__HassTurnOn"})]);
    let mut client = make_client(&transport);

    let data = execute(&mut client, "HassTurnOn", &json!({"area": "Кухня"})).unwrap();

    assert_eq!(data["ok"], json!(true));
    assert_eq!(transport.tool_calls()[0].0, "assist__HassTurnOn");
}

#[test]
fn execute_adapts_string_to_array_from_tool_schema() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({
        "name": "intent__HassTurnOn",
        "inputSchema": {
            "type": "object",
            "properties": {"domain": {"type": "array"}},
        },
    })]);
    let mut client = make_client(&transport);

    execute(
        &mut client,
        "HassTurnOn",
        &json!({"area": "Кухня", "domain": "light"}),
    )
    .unwrap();

    assert_eq!(
        transport.tool_calls(),
        vec![(
            "intent__HassTurnOn".to_string(),
            json!({"area": "Кухня", "domain": ["light"]}),
        )]
    );
}

#[test]
fn execute_preserves_existing_array_argument() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({
        "name": "HassTurnOn",
        "inputSchema": {
            "properties": {"domain": {"type": "array"}},
        },
    })]);
    let mut client = make_client(&transport);

    execute(
        &mut client,
        "HassTurnOn",
        &json!({"domain": ["light", "switch"]}),
    )
    .unwrap();

    assert_eq!(
        transport.tool_calls()[0].1,
        json!({"domain": ["light", "switch"]}),
    );
}

#[test]
fn execute_stale_tool_result_triggers_refresh_and_retry() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "old__HassTurnOn"})]);
    transport.push_tools_list(vec![json!({"name": "new__HassTurnOn"})]);
    transport.push_call_result(
        200,
        json!({
            "isError": true,
            "content": [{"type": "text", "text": "Error calling tool: Tool \"old__HassTurnOn\" not found"}],
        }),
    );
    let mut client = make_client(&transport);

    let data = execute(&mut client, "HassTurnOn", &json!({"area": "Кухня"})).unwrap();

    assert_eq!(data["ok"], json!(true));
    // tools/list: изначальный + refresh; tools/call: stale + retry.
    assert_eq!(transport.tools_list_calls(), 2);
    assert_eq!(transport.tool_calls().len(), 2);
    assert_eq!(transport.tool_calls()[1].0, "new__HassTurnOn");
}

#[test]
fn get_state_fallback_context_error_propagates_on_bad_text() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "homeassistant__GetLiveContext"})]);
    let mut client = make_client(&transport);

    let err = execute(
        &mut client,
        "HassGetState",
        &json!({"area": "кухня", "domain": "switch", "name": "light_kitchen"}),
    )
    .unwrap_err();

    assert_eq!(err.kind.as_str(), ErrorType::Context.as_str());
    // Ответ мока ("Turned on") — не JSON, парсер live context падает.
    assert_eq!(
        transport.tool_calls(),
        vec![("homeassistant__GetLiveContext".to_string(), json!({}))],
    );
}

// Полный порт test_get_state_falls_back_to_live_context_when_tool_is_missing:
// требует живой реализации context (Phase 4).
#[test]
fn get_state_falls_back_to_live_context_when_tool_is_missing() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "homeassistant__GetLiveContext"})]);
    transport.push_call_result(
        200,
        json!({
            "content": [{
                "type": "text",
                "text": r#"{"entities":[{"area":"kitchen, кухня","domain":"switch","name":"light_kitchen","state":"off"}]}"#,
            }],
            "isError": false,
        }),
    );
    let mut client = make_client(&transport);

    let result = execute(
        &mut client,
        "HassGetState",
        &json!({"area": "кухня", "domain": "switch", "name": "light_kitchen"}),
    )
    .unwrap();

    assert_eq!(result["response_type"], json!("query_answer"));
    assert_eq!(result["data"]["states"][0]["state"], json!("off"));
    assert_eq!(
        transport.tool_calls(),
        vec![("homeassistant__GetLiveContext".to_string(), json!({}))],
    );
}

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
fn semantic_payload_still_works() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "HassTurnOn"})]);
    let mut client = make_client(&transport);

    let data = execute(
        &mut client,
        "HassTurnOn",
        &json!({"area": "Кухня", "domain": "light", "name": "Ceiling"}),
    )
    .unwrap();

    assert_eq!(data["ok"], json!(true));
    assert_eq!(
        transport.tool_calls(),
        vec![(
            "HassTurnOn".to_string(),
            json!({"area": "Кухня", "domain": "light", "name": "Ceiling"}),
        )]
    );
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
