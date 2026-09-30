use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::context;
use ha_cli::errors::{ErrorType, HaCliError};
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

// ---------- Mock-транспорт (аналог FakeClient из test_context.py) ----------

#[derive(Clone)]
struct MockTransport {
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

type Resp = (u16, Vec<(String, String)>, String);

fn rpc_result(result: &Json, id: &Json) -> Resp {
    (
        200,
        Vec::new(),
        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    )
}

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

// ---------- get_live_context / refresh ----------

#[test]
fn get_live_context_namespace_independent() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "srv1__GetLiveContext"})]);
    transport.push_call_result(
        200,
        json!({
            "content": [{
                "type": "text",
                "text": r#"{"entities": [{"entity_id": "light.kitchen", "name": "Kitchen", "domain": "light", "area": "Kitchen"}]}"#,
            }],
            "isError": false,
        }),
    );
    let mut client = make_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();

    assert_eq!(live["entities"][0]["entity_id"], json!("light.kitchen"));
    assert_eq!(
        transport.tool_calls(),
        vec![("srv1__GetLiveContext".to_string(), json!({}))],
    );
}

#[test]
fn get_live_context_prefers_structured_content() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "GetLiveContext"})]);
    transport.push_call_result(
        200,
        json!({
            "content": [{"type": "text", "text": r#"{"entities": []}"#}],
            "isError": false,
            "structuredContent": {"entities": [{"entity_id": "light.bed"}]},
        }),
    );
    let mut client = make_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();

    assert_eq!(live, json!({"entities": [{"entity_id": "light.bed"}]}));
}

#[test]
fn get_live_context_refreshes_stale_tool_name() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "old__GetLiveContext"})]);
    // Прогрев кэша (аналог ToolDiscovery(tools_client(...)).tools()).
    let mut client = make_client(&transport);
    ha_cli::discovery::ToolDiscovery::new(None)
        .tools(&mut client)
        .unwrap();

    let transport = MockTransport::new(vec![json!({"name": "homeassistant__GetLiveContext"})]);
    transport.push_call_result(
        200,
        json!({
            "isError": true,
            "content": [{
                "type": "text",
                "text": r#"Error calling tool: Tool "old__GetLiveContext" not found"#,
            }],
        }),
    );
    transport.push_call_result(
        200,
        json!({
            "isError": false,
            "content": [{"type": "text", "text": r#"{"entities": []}"#}],
        }),
    );
    let mut client = make_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();

    assert_eq!(live, json!({"entities": []}));
    let names: Vec<String> = transport
        .tool_calls()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        vec![
            "old__GetLiveContext".to_string(),
            "homeassistant__GetLiveContext".to_string(),
        ]
    );
}

// ---------- parse_live_context ----------

#[test]
fn parse_rejects_is_error() {
    let err = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": "no agent"}],
        "isError": true,
    }))
    .unwrap_err();
    assert!(err.message.contains("no agent"));
    assert_eq!(err.kind.as_str(), ErrorType::Context.as_str());
}

#[test]
fn parse_rejects_non_dict() {
    let result = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": "[1, 2]"}],
        "isError": false,
    }));
    assert!(result.is_err());
}

#[test]
fn parse_rejects_invalid_json_text() {
    let result = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": "not json"}],
        "isError": false,
    }));
    assert!(result.is_err());
}

#[test]
fn parse_rejects_missing_content() {
    assert!(context::parse_live_context(&json!({})).is_err());
}

#[test]
fn parse_rejects_non_dict_result() {
    assert!(context::parse_live_context(&json!("ok")).is_err());
}

#[test]
fn parse_rejects_structured_content_not_object() {
    let result = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": "{}"}],
        "isError": false,
        "structuredContent": [1, 2],
    }));
    assert!(result.is_err());
}

#[test]
fn parse_rejects_empty_text() {
    assert!(context::parse_live_context(&json!({"content": [], "isError": false})).is_err());
}

#[test]
fn parse_is_error_without_text_falls_back_to_default_message() {
    let err = context::parse_live_context(&json!({
        "content": [],
        "isError": true,
    }))
    .unwrap_err();
    assert_eq!(err.message, "context request failed");
}

#[test]
fn parse_current_home_assistant_yaml_envelope() {
    let text = json!({
        "success": true,
        "result": "Live Context:\n- names: kitchen: light\n  domain: switch\n  state: 'off'\n  areas: kitchen\n  attributes:\n    device_class: outlet\n",
    })
    .to_string();

    let parsed = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
    .unwrap();

    assert_eq!(
        parsed,
        json!({
            "entities": [{
                "name": "kitchen: light",
                "area": "kitchen",
                "domain": "switch",
                "state": "off",
                "capabilities": {"device_class": "outlet"},
            }],
        })
    );
}

#[test]
fn parse_current_context_with_first_entity_on_header_line() {
    let text = json!({
        "success": true,
        "result": "Live Context: - names: kitchen light\n  domain: switch\n  state: 'off'\n  areas: kitchen",
    })
    .to_string();
    let parsed = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(parsed["entities"][0]["name"], json!("kitchen light"));
}

#[test]
fn parse_current_context_ignores_header_description() {
    let text = json!({
        "success": true,
        "result": "Live Context: An overview of the smart home\n- names: kitchen light\n  domain: switch\n  state: 'off'",
    })
    .to_string();
    let parsed = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(parsed["entities"][0]["name"], json!("kitchen light"));
}

#[test]
fn parse_rejects_failed_home_assistant_envelope() {
    let text = r#"{"success": false, "result": "failed"}"#;
    let result = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    }));
    assert!(result.is_err());
}

#[test]
fn parse_rejects_non_text_wrapped_result() {
    let result = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": r#"{"success": true, "result": 42}"#}],
        "isError": false,
    }));
    let err = result.unwrap_err();
    assert_eq!(err.message, "GetLiveContext result is not text");
}

#[test]
fn parse_rejects_unknown_text_format() {
    let result = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Something else entirely"}"#,
        }],
        "isError": false,
    }));
    let err = result.unwrap_err();
    assert_eq!(err.message, "GetLiveContext result has an unknown format");
}

#[test]
fn parse_returns_payload_without_envelope() {
    let parsed = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": r#"{"entities": [{"name": "A"}]}"#}],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(parsed, json!({"entities": [{"name": "A"}]}));
}

// ---------- build_arguments ----------

#[test]
fn build_arguments_empty_schema() {
    assert_eq!(context::build_arguments(&json!({})).unwrap(), json!({}));
}

#[test]
fn build_arguments_uses_defaults() {
    let schema = json!({
        "properties": {"verbose": {"type": "boolean", "default": true}},
    });
    assert_eq!(
        context::build_arguments(&schema).unwrap(),
        json!({"verbose": true})
    );
}

#[test]
fn build_arguments_required_without_default_raises() {
    let schema = json!({
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
    });
    let err = context::build_arguments(&schema).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Context.as_str());
    assert_eq!(
        err.message,
        "GetLiveContext requires argument 'name' with no default value"
    );
}

#[test]
fn build_arguments_non_object_properties() {
    let schema = json!({"properties": "oops"});
    assert_eq!(context::build_arguments(&schema).unwrap(), json!({}));
}

// ---------- Текстовый парсер ----------

#[test]
fn parser_multi_area_and_capabilities() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n- names: kitchen light\n  domain: light\n  areas: Kitchen, Кухня\n  state: 'on'\n    friendly_name: Kitchen\n    brightness: 200\n- names: plug\n  areas: Kitchen\n  domain: switch"}"#,
        }],
        "isError": false,
    }))
    .unwrap();

    assert_eq!(
        parsed,
        json!({
            "entities": [
                {
                    "name": "kitchen light",
                    "area": "Kitchen, Кухня",
                    "domain": "light",
                    "state": "on",
                    "capabilities": {
                        "friendly_name": "Kitchen",
                        "brightness": "200",
                    },
                },
                {
                    "name": "plug",
                    "area": "Kitchen",
                    "domain": "switch",
                    "state": null,
                    "capabilities": {},
                },
            ],
        })
    );
}

#[test]
fn parser_quoted_values_and_double_quotes() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n- names: 'kitchen light'\n  state: \"off\"\n  domain: light"}"#,
        }],
        "isError": false,
    }))
    .unwrap();

    assert_eq!(parsed["entities"][0]["name"], json!("kitchen light"));
    assert_eq!(parsed["entities"][0]["state"], json!("off"));
}

#[test]
fn parser_ignores_attributes() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n- names: lamp\n  domain: light\n  areas: Kitchen\n  attributes:\n    supported: yes"}"#,
        }],
        "isError": false,
    }))
    .unwrap();

    let entity = &parsed["entities"][0];
    assert_eq!(entity["area"], json!("Kitchen"));
    // Сам ключ attributes игнорируется, вложенные строки уходят в capabilities.
    assert!(entity.get("attributes").is_none());
    assert_eq!(entity["capabilities"], json!({"supported": "yes"}));
}

#[test]
fn parser_skips_blank_lines() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n\n- names: lamp\n\n  domain: light\n"}"#,
        }],
        "isError": false,
    }))
    .unwrap();

    assert_eq!(parsed["entities"][0]["domain"], json!("light"));
}

#[test]
fn parser_unknown_field_goes_to_capabilities() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n- names: lamp\n  domain: light\n  color: warm"}"#,
        }],
        "isError": false,
    }))
    .unwrap();

    assert_eq!(
        parsed["entities"][0]["capabilities"],
        json!({"color": "warm"})
    );
}

#[test]
fn parser_rejects_field_without_current_entity() {
    let result = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context: an overview\n  domain: light"}"#,
        }],
        "isError": false,
    }));
    let err = result.unwrap_err();
    assert_eq!(err.message, "GetLiveContext entity has an unknown format");
}

#[test]
fn parser_rejects_field_without_colon() {
    let result = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n- names: lamp\n  broken line"}"#,
        }],
        "isError": false,
    }));
    let err = result.unwrap_err();
    assert_eq!(err.message, "GetLiveContext field has an unknown format");
}

#[test]
fn parser_empty_entities() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:"}"#,
        }],
        "isError": false,
    }))
    .unwrap();

    assert_eq!(parsed, json!({"entities": []}));
}

// ---------- normalize_entities ----------

#[test]
fn normalize_entities_skips_malformed() {
    let entities = context::normalize_entities(&json!({
        "entities": [
            null,
            5,
            {"name": 42},
            {"entity_id": "light.technical", "domain": "light", "area": "Kitchen"},
            {"name": "Kitchen Light", "domain": "light", "area": "Kitchen"},
        ],
    }));
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].name, "Kitchen Light");
    assert_eq!(entities[0].area, "Kitchen");
    assert_eq!(entities[0].domain, "light");
}

#[test]
fn normalize_entities_defaults_and_capabilities() {
    let entities = context::normalize_entities(&json!({
        "entities": [{
            "name": "A",
            "state": "on",
            "capabilities": {"device_class": "outlet"},
            "extra": "junk",
        }],
    }));
    assert_eq!(entities[0].area, "Unknown");
    assert_eq!(entities[0].domain, "unknown");
    assert_eq!(entities[0].state, Some("on".to_string()));
    assert_eq!(entities[0].capabilities, json!({"device_class": "outlet"}));
}

#[test]
fn normalize_entities_non_array_is_empty() {
    assert!(context::normalize_entities(&json!({"entities": "oops"})).is_empty());
    assert!(context::normalize_entities(&json!({})).is_empty());
}

// ---------- build_context ----------

#[test]
fn build_context_groups_by_area() {
    let built = context::build_context(&json!({
        "entities": [
            {
                "entity_id": "light.kitchen",
                "name": "Kitchen Light",
                "domain": "light",
                "area": "Kitchen",
            },
            {
                "entity_id": "switch.plug",
                "name": "Plug",
                "domain": "switch",
                "area": "Kitchen",
            },
            {"entity_id": "sensor.temp", "name": "Temp", "domain": "sensor"},
        ],
    }));
    assert_eq!(
        built,
        json!({
            "areas": {
                "Kitchen": {
                    "light": ["Kitchen Light"],
                    "switch": ["Plug"],
                },
                "Unknown": {"sensor": ["Temp"]},
            }
        })
    );
}

#[test]
fn build_context_tolerates_bad_entities() {
    let built = context::build_context(&json!({"entities": "oops", "x": 1}));
    assert_eq!(built, json!({"areas": {}}));
    let built = context::build_context(&json!({"entities": [null, 5, {"name": "A"}]}));
    assert_eq!(built, json!({"areas": {"Unknown": {"unknown": ["A"]}}}));
}

#[test]
fn build_context_splits_area_aliases() {
    let built = context::build_context(&json!({
        "entities": [{
            "name": "light_livingroom",
            "domain": "switch",
            "area": "livingroom, hall, гостиная, зал",
        }],
    }));
    assert_eq!(
        built,
        json!({
            "areas": {
                "livingroom": {"switch": ["light_livingroom"]},
            },
            "area_aliases": {
                "livingroom": ["hall", "гостиная", "зал"],
            },
        })
    );
}

#[test]
fn build_context_deterministic_order_and_dedup() {
    let built = context::build_context(&json!({
        "entities": [
            {"name": "B", "domain": "light", "area": "Kitchen"},
            {"name": "A", "domain": "light", "area": "Kitchen"},
            {"name": "A", "domain": "light", "area": "Kitchen"},
            {"name": "C", "domain": "switch", "area": "Attic"},
        ],
    }));
    let areas = &built["areas"];
    let area_keys: Vec<&str> = areas
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(area_keys, vec!["Attic", "Kitchen"]);
    let kitchen_domains: Vec<&str> = areas["Kitchen"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(kitchen_domains, vec!["light"]);
    assert_eq!(areas["Kitchen"]["light"], json!(["A", "B"]));
    assert!(built.get("area_aliases").is_none());
}

#[test]
fn build_context_aliases_sorted_and_deduped() {
    let built = context::build_context(&json!({
        "entities": [
            {"name": "E", "domain": "switch", "area": "hall, зал, hall"},
            {"name": "F", "domain": "switch", "area": "hall"},
        ],
    }));
    assert_eq!(built["areas"]["hall"]["switch"], json!(["E", "F"]));
    assert_eq!(built["area_aliases"], json!({"hall": ["зал"]}));
}

// ---------- compact_context / size_report ----------

#[test]
fn compact_context_keeps_only_flat_areas() {
    let context_full = json!({
        "areas": {"Kitchen": {"light": ["A"], "switch": ["B"]}},
        "area_aliases": {"Kitchen": ["Cooking"]},
    });
    assert_eq!(
        context::compact_context(&context_full),
        json!({"Kitchen": {"light": ["A"], "switch": ["B"]}}),
    );
}

#[test]
fn compact_context_has_no_technical_fields() {
    let live = json!({
        "entities": [{
            "name": "A",
            "domain": "light",
            "area": "Kitchen",
            "state": "on",
            "capabilities": {"device_class": "outlet"},
            "entity_id": "light.a",
        }],
    });
    let compact = context::compact_context(&context::build_context(&live));
    let serialized = compact.to_string();
    assert!(!serialized.contains("state"));
    assert!(!serialized.contains("capabilities"));
    assert!(!serialized.contains("entity_id"));
    assert_eq!(compact, json!({"Kitchen": {"light": ["A"]}}));
}

#[test]
fn compact_context_missing_areas_is_empty() {
    assert_eq!(context::compact_context(&json!({})), json!({}));
}

#[test]
fn size_report_compact_not_larger() {
    let live = json!({
        "entities": [{
            "name": "A",
            "domain": "light",
            "area": "Kitchen",
            "state": "on",
            "capabilities": {"device_class": "outlet"},
        }],
    });
    let report = context::size_report(&context::build_context(&live));
    assert!(report["compact"].as_u64().unwrap() < report["full"].as_u64().unwrap());
    assert!(report["full"].as_u64().unwrap() > 0);
}

#[test]
fn size_report_shape() {
    let report = context::size_report(&json!({"areas": {}}));
    assert_eq!(
        report,
        json!({
            "full": context::serialized_size(&json!({"ok": true, "areas": {}})),
            "compact": context::serialized_size(&json!({"ok": true})),
        })
    );
}

// ---------- query_state ----------

#[test]
fn query_state_matches_area_alias_and_semantic_name() {
    let result = context::query_state(
        &json!({
            "entities": [{
                "area": "kitchen, кухня",
                "domain": "switch",
                "name": "light_kitchen",
                "state": "on",
            }],
        }),
        &json!({"area": "КУХНЯ", "domain": "switch", "name": "light_kitchen"}),
    )
    .unwrap();
    assert_eq!(
        result,
        json!({
            "ok": true,
            "response_type": "query_answer",
            "speech": "light_kitchen: on",
            "data": {
                "states": [{
                    "area": "kitchen",
                    "domain": "switch",
                    "name": "light_kitchen",
                    "state": "on",
                }],
            },
        })
    );
}

#[test]
fn query_state_requires_semantic_target() {
    let err = context::query_state(&json!({"entities": []}), &json!({})).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::InvalidArguments.as_str());
}

#[test]
fn query_state_ignores_empty_string_selectors() {
    // Пустая строка в Python falsy -> селектор не учитывается.
    let err = context::query_state(&json!({"entities": []}), &json!({"area": ""})).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::InvalidArguments.as_str());
    // Строка из пробелов truthy -> селектор учитывается (нет совпадений).
    let err = context::query_state(&json!({"entities": []}), &json!({"name": "   "})).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Intent.as_str());
}

#[test]
fn query_state_reports_no_match() {
    let err =
        context::query_state(&json!({"entities": []}), &json!({"area": "kitchen"})).unwrap_err();
    assert!(err.message.contains("no exposed entity"));
    assert_eq!(err.kind.as_str(), ErrorType::Intent.as_str());
}

#[test]
fn query_state_unknown_state_becomes_unknown_in_speech() {
    let result = context::query_state(
        &json!({"entities": [{"name": "lamp", "domain": "light", "area": "Kitchen"}]}),
        &json!({"area": "kitchen"}),
    )
    .unwrap();
    assert_eq!(result["speech"], json!("lamp: unknown"));
    assert_eq!(result["data"]["states"][0]["state"], Json::Null);
}

#[test]
fn query_state_matches_multiple_entities() {
    let result = context::query_state(
        &json!({
            "entities": [
                {"name": "A", "domain": "light", "area": "Kitchen", "state": "on"},
                {"name": "B", "domain": "switch", "area": "Attic", "state": "off"},
            ],
        }),
        &json!({"area": "Kitchen"}),
    )
    .unwrap();
    assert_eq!(result["speech"], json!("A: on"));
    assert_eq!(result["data"]["states"].as_array().unwrap().len(), 1);
}
