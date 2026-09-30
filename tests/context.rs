use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::context;
use ha_cli::errors::{ErrorType, HaCliError};
use ha_cli::models::Entity;
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

/// Клиент с настроенным `mcp_url`: путь ha-mcp (ha_search вместо
/// GetLiveContext).
fn make_mcp_client(transport: &MockTransport) -> Client {
    let secrets = Secrets::new();
    let config = Config {
        url: None,
        mcp_url: Some("http://ha.test/api/webhook/testsecret123".to_string()),
        mcp_auth: Default::default(),
        token: String::new(),
        timeout: 5,
        connect_timeout: 5,
    };
    Client::new(config, Box::new(transport.clone()), &secrets)
}

// ---------- ha_search (этап 3.1: семантическое разрешение, ha-mcp) ----------

fn search_page(entities: Json, total: i64, has_more: bool, next_offset: Json) -> Json {
    json!({
        "success": true,
        "entities": entities,
        "entity_total_matches": total,
        "entity_has_more": has_more,
        "entity_next_offset": next_offset,
        "partial": false,
        "errors": [],
        "warnings": [],
        "offset": 0,
        "limit": 1,
    })
}

fn entity(id: &str, name: &str, area: &str) -> Json {
    json!({
        "entity_id": id,
        "friendly_name": name,
        "domain": id.split('.').next().unwrap_or("unknown"),
        "state": "on",
        "area": area,
        "aliases": [],
    })
}

/// Обзор с доменами; домены перебираются в отсортированном порядке.
fn domain_stats(stats: Json) -> Json {
    json!({"success": true, "domain_stats": stats, "entities": [{"entity_id": "overview.leak"}]})
}

fn ha_search_transport(overview: Json) -> MockTransport {
    let transport = MockTransport::new(vec![
        json!({"name": "ha_search"}),
        json!({"name": "ha_get_overview"}),
    ]);
    // Первый tools/call — ha_get_overview(fields=["domain_stats"]).
    transport.push_call_result(200, json!({"structuredContent": overview}));
    transport
}

#[test]
fn ha_search_aggregates_all_domains_with_per_domain_offsets() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({
        "sensor": 1,
        "light": 2,
    })));
    // Домены в отсортированном порядке: light (2 страницы), sensor (1).
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "Bathroom")]),
                2,
                true,
                json!(1),
            ),
        }),
    );
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.b", "B", "")]),
                2,
                false,
                Json::Null,
            ),
        }),
    );
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("sensor.c", "C", "Bathroom")]),
                1,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();

    assert_eq!(live["entities"].as_array().unwrap().len(), 3);
    assert_eq!(live["entity_total_matches"], json!(3));
    assert_eq!(live["partial"], json!(false));
    assert_eq!(live["source"], json!("ha_search"));
    assert_eq!(live["domains"], json!(2));
    // Сущность из обзора не протекает в каталог.
    assert!(live["entities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["entity_id"] != json!("overview.leak")));
    let calls = transport.tool_calls();
    let (overview_calls, search_calls): (Vec<_>, Vec<_>) = calls
        .iter()
        .cloned()
        .partition(|(name, _)| name == "ha_get_overview");
    // Обзор запрошен ровно один раз с проекцией domain_stats.
    assert_eq!(overview_calls.len(), 1);
    assert_eq!(overview_calls[0].1["fields"], json!(["domain_stats"]));
    // Каждый ha_search — только с непустым domain_filter, limit и
    // result_fields; offset сбрасывается на каждом домене.
    assert_eq!(search_calls.len(), 3);
    assert!(search_calls.iter().all(|(_, args)| {
        args["domain_filter"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
            && args["limit"] == json!(context::HA_SEARCH_PAGE_LIMIT)
            && args["result_fields"].is_array()
    }));
    let sequence: Vec<(String, i64)> = search_calls
        .iter()
        .map(|(_, args)| {
            (
                args["domain_filter"].as_str().unwrap().to_string(),
                args["offset"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            ("light".to_string(), 0),
            ("light".to_string(), 1),
            ("sensor".to_string(), 0),
        ]
    );
}

#[test]
fn ha_search_accepts_overview_and_pages_as_json_text() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    let page = search_page(
        json!([entity("light.a", "A", "Kitchen")]),
        1,
        false,
        Json::Null,
    );
    transport.push_call_result(
        200,
        json!({"content": [{"type": "text", "text": page.to_string()}]}),
    );
    let mut client = make_mcp_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();

    assert_eq!(live["entities"].as_array().unwrap().len(), 1);
    assert_eq!(live["entities"][0]["entity_id"], json!("light.a"));
}

#[test]
fn ha_search_context_and_compact_use_friendly_names() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 2})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([
                    entity("light.main", "Main Light", "Bathroom"),
                    {"entity_id": "light.alt", "friendly_name": "Alt", "domain": "light", "state": "off", "area": "Bathroom", "aliases": ["Sanuzel"]},
                ]),
                2,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();
    let built = context::build_context(&live);

    assert_eq!(
        built,
        json!({
            "areas": {"Bathroom": {"light": ["Alt", "Main Light"]}},
        })
    );
}

#[test]
fn ha_search_deduplicates_entity_ids_across_pages() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 2})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                2,
                true,
                json!(1),
            ),
        }),
    );
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X"), entity("light.b", "B", "X")]),
                2,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);

    let live = context::get_live_context(&mut client).unwrap();

    let ids: Vec<&str> = live["entities"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["entity_id"].as_str())
        .collect();
    assert_eq!(ids, vec!["light.a", "light.b"]);
}

#[test]
fn ha_search_domain_stats_as_object_map_is_supported() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                1,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);
    assert!(context::get_live_context(&mut client).is_ok());
}

#[test]
fn ha_search_domain_stats_as_array_of_objects_is_supported() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!([
        {"domain": "light", "count": 1},
    ])));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                1,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);
    assert!(context::get_live_context(&mut client).is_ok());
}

#[test]
fn ha_search_domain_stats_nested_under_overview_key_is_supported() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(json!({
        "success": true,
        "overview": {"domain_stats": {"light": 1}},
    }));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                1,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);
    assert!(context::get_live_context(&mut client).is_ok());
}

#[test]
fn ha_search_partial_result_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    let page = search_page(json!([entity("light.a", "A", "X")]), 1, false, Json::Null)
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                if k == "partial" {
                    json!(true)
                } else {
                    v.clone()
                },
            )
        })
        .collect::<Json>();
    transport.push_call_result(200, json!({"structuredContent": page}));
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("partial"));
}

#[test]
fn ha_search_errors_field_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    let mut page = search_page(json!([]), 0, false, Json::Null);
    page["errors"] = json!(["internal failure"]);
    transport.push_call_result(200, json!({"structuredContent": page}));
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("incomplete"));
    // Детали ошибок сервера не выводятся.
    assert!(!err.message.contains("internal failure"));
}

#[test]
fn ha_search_has_more_without_next_offset_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 3})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                3,
                true,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("entity_next_offset"));
}

#[test]
fn ha_search_collected_entities_missing_total_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 5})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                5,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("1 of 5"));
}

#[test]
fn ha_search_tool_is_error_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    transport.push_call_result(
        200,
        json!({
            "isError": true,
            "content": [{"type": "text", "text": "ha_search failed"}],
        }),
    );
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("ha_search failed"));
}

#[test]
fn ha_search_success_false_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": {"success": false, "error": "search unavailable"},
        }),
    );
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("search unavailable"));
}

#[test]
fn ha_search_overview_is_error_is_rejected() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![
        json!({"name": "ha_search"}),
        json!({"name": "ha_get_overview"}),
    ]);
    transport.push_call_result(
        200,
        json!({
            "isError": true,
            "content": [{"type": "text", "text": "overview unavailable"}],
        }),
    );
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("overview unavailable"));
}

#[test]
fn ha_search_missing_domain_stats_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(json!({"success": true}));
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("domain_stats"));
}

#[test]
fn ha_search_empty_domain_stats_is_rejected() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({})));
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::Context));
    assert!(err.message.contains("empty domain_stats"));
}

#[test]
fn ha_search_raw_returns_aggregated_catalog() {
    let _cache = isolated_cache();
    let transport = ha_search_transport(domain_stats(json!({"light": 1})));
    transport.push_call_result(
        200,
        json!({
            "structuredContent": search_page(
                json!([entity("light.a", "A", "X")]),
                1,
                false,
                Json::Null,
            ),
        }),
    );
    let mut client = make_mcp_client(&transport);

    let raw = context::get_raw_result(&mut client).unwrap();

    assert_eq!(raw["entities"].as_array().unwrap().len(), 1);
    assert_eq!(raw["partial"], json!(false));
}

#[test]
fn ha_search_missing_tool_is_not_found_error() {
    let _cache = isolated_cache();
    let transport = MockTransport::new(vec![json!({"name": "other_tool"})]);
    let mut client = make_mcp_client(&transport);

    let err = context::get_live_context(&mut client).unwrap_err();

    assert!(matches!(err.kind, ErrorType::ToolNotFound));
}

#[test]
fn entity_from_raw_supports_ha_search_and_assist_shapes() {
    let ha = Entity::from_raw(&json!({
        "entity_id": "light.a",
        "friendly_name": "A",
        "domain": "light",
        "state": "on",
        "area": "Bathroom",
        "aliases": ["Sanuzel", "Bathroom"],
    }))
    .unwrap();
    assert_eq!(ha.name, "A");
    assert_eq!(ha.area, "Bathroom");
    assert_eq!(ha.aliases, Vec::<String>::new());
    // Алиасы сущности сохраняются целиком (в т.ч. совпадающие с областью):
    // это алиасы сущности, а не области.
    assert_eq!(
        ha.entity_aliases,
        vec!["Sanuzel".to_string(), "Bathroom".to_string()]
    );
    assert_eq!(ha.entity_id.as_deref(), Some("light.a"));

    let assist = Entity::from_raw(&json!({
        "name": "B",
        "domain": "switch",
        "area": "Kitchen,Cooking",
    }))
    .unwrap();
    assert_eq!(assist.area, "Kitchen");
    assert_eq!(assist.aliases, vec!["Cooking".to_string()]);
    assert_eq!(assist.entity_id, None);
    assert!(Entity::from_raw(&json!({"friendly_name": ""})).is_none());
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

// ---------- 2.4: ошибки без content и structured success:false ----------

#[test]
fn parse_is_error_without_content_uses_structured_tool_error() {
    let err = context::parse_live_context(&json!({
        "isError": true,
        "structuredContent": {"error": "agent unavailable"},
    }))
    .unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Context.as_str());
    assert_eq!(err.message, "agent unavailable");
}

#[test]
fn parse_is_error_without_content_and_structured_keeps_default_message() {
    let err = context::parse_live_context(&json!({"isError": true})).unwrap_err();
    assert_eq!(err.message, "context request failed");
}

#[test]
fn parse_structured_success_false_is_context_error() {
    let err = context::parse_live_context(&json!({
        "content": [],
        "isError": false,
        "structuredContent": {"success": false, "error": "context refused"},
    }))
    .unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Context.as_str());
    assert_eq!(err.message, "context refused");
}

#[test]
fn parse_structured_success_false_without_error_has_default_message() {
    let err = context::parse_live_context(&json!({
        "content": [],
        "isError": false,
        "structuredContent": {"success": false},
    }))
    .unwrap_err();
    assert_eq!(err.message, "GetLiveContext reported failure");
}

#[test]
fn parse_structured_success_true_is_success() {
    let parsed = context::parse_live_context(&json!({
        "content": [],
        "isError": false,
        "structuredContent": {"success": true, "entities": [{"name": "A"}]},
    }))
    .unwrap();
    assert_eq!(parsed["entities"][0]["name"], json!("A"));
}

#[test]
fn parse_text_success_false_without_result_is_context_error() {
    let err = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": r#"{"success": false, "error": "boom"}"#}],
        "isError": false,
    }))
    .unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Context.as_str());
    assert_eq!(err.message, "boom");
}

#[test]
fn parse_text_success_false_with_result_still_fails() {
    let result = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": r#"{"success": false, "result": "failed"}"#}],
        "isError": false,
    }));
    assert_eq!(
        result.unwrap_err().message,
        "GetLiveContext reported failure"
    );
}

#[test]
fn parse_legacy_success_true_envelope_still_unwraps() {
    let parsed = context::parse_live_context(&json!({
        "content": [{
            "type": "text",
            "text": r#"{"success": true, "result": "Live Context:\n- names: lamp\n  domain: light"}"#,
        }],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(parsed["entities"][0]["name"], json!("lamp"));
}

#[test]
fn parse_result_without_success_is_returned_as_is() {
    // Регрессия: конверт с result, но без success — payload как есть,
    // без попытки парсить "Live Context:".
    let parsed = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": r#"{"result": "raw payload"}"#}],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(parsed, json!({"result": "raw payload"}));
}

#[test]
fn parse_success_without_result_is_returned_as_is() {
    let parsed = context::parse_live_context(&json!({
        "content": [{"type": "text", "text": r#"{"success": true, "entities": []}"#}],
        "isError": false,
    }))
    .unwrap();
    assert_eq!(parsed, json!({"success": true, "entities": []}));
}
