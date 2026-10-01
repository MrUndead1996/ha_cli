use clap::Parser as _;
use ha_cli::cli::{dispatch, Cli};
use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::errors::{
    ErrorType, HaCliError, EXIT_AUTHENTICATION_ERROR, EXIT_CONFIGURATION_ERROR,
    EXIT_CONNECTION_ERROR, EXIT_CONTEXT_ERROR, EXIT_INTENT_EXECUTION_ERROR, EXIT_INVALID_ARGUMENTS,
    EXIT_TOOL_NOT_FOUND,
};
use ha_cli::security::Secrets;
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

type Resp = Result<HttpResponse, HaCliError>;
type Handler = dyn Fn(&Json) -> Resp + Send + Sync;

/// Аналог FakeClient из Python test_cli.py: подменяемый транспорт.
#[derive(Clone)]
struct FakeTransport {
    calls: Arc<Mutex<Vec<(String, Json)>>>,
    handler: Arc<Handler>,
}

struct Fake {
    transport: FakeTransport,
}

impl Fake {
    /// `call_results` — очередь ответов tools/call: извлекаются по порядку,
    /// последний повторяется (аналог последовательных ответов сервера).
    fn new(tools: Json, call_results: Vec<Json>, error: Option<HaCliError>) -> Self {
        let calls: Arc<Mutex<Vec<(String, Json)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_handler = Arc::clone(&calls);
        let call_results = Arc::new(Mutex::new(std::collections::VecDeque::from(call_results)));
        let results_handler = Arc::clone(&call_results);
        let handler: Arc<Handler> = Arc::new(move |payload: &Json| {
            if let Some(err) = &error {
                return Err(err.clone());
            }
            match payload["method"].as_str().unwrap() {
                "initialize" => Ok(rpc_result(
                    &json!({"protocolVersion": "2025-03-26", "capabilities": {}}),
                    payload["id"].as_u64().unwrap(),
                )),
                "notifications/initialized" => Ok(empty_202()),
                "tools/list" => Ok(rpc_result(
                    &json!({"tools": tools}),
                    payload["id"].as_u64().unwrap(),
                )),
                "tools/call" => {
                    calls_handler.lock().unwrap().push((
                        payload["params"]["name"].as_str().unwrap().to_string(),
                        payload["params"]["arguments"].clone(),
                    ));
                    let result = {
                        let mut queue = results_handler.lock().unwrap();
                        match queue.pop_front() {
                            Some(result) => {
                                queue.push_back(result.clone());
                                result
                            }
                            None => queue.back().cloned().unwrap_or(json!(null)),
                        }
                    };
                    Ok(rpc_result(&result, payload["id"].as_u64().unwrap()))
                }
                other => panic!("unexpected method {other}"),
            }
        });
        Self {
            transport: FakeTransport { calls, handler },
        }
    }

    fn calls(&self) -> Vec<(String, Json)> {
        self.transport.calls.lock().unwrap().clone()
    }
}

impl Transport for FakeTransport {
    fn post(
        &mut self,
        _url: &str,
        payload: &Json,
        _headers: &[(String, String)],
    ) -> Result<HttpResponse, HaCliError> {
        (self.handler)(payload)
    }
}

fn rpc_result(result: &Json, id: u64) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    }
}

fn empty_202() -> HttpResponse {
    HttpResponse {
        status: 202,
        headers: Vec::new(),
        body: String::new(),
    }
}

fn default_tools() -> Json {
    json!([
        {"name": "ha_get_overview"},
        {"name": "ha_search"},
        {"name": "ha_get_state"},
        {"name": "ha_call_service"},
    ])
}

/// Ответ ha_get_overview с одним доменом `light`.
fn overview(domains: Json) -> Json {
    json!({"structuredContent": {"success": true, "domain_stats": domains}})
}

/// Страница ha_search с переданными сущностями (total = entities.len()).
fn search_page(entities: Json) -> Json {
    let total = entities.as_array().map(Vec::len).unwrap_or(0) as i64;
    json!({"structuredContent": {
        "success": true,
        "entities": entities,
        "entity_total_matches": total,
        "partial": false,
        "errors": [],
        "entity_has_more": false,
        "entity_next_offset": null,
    }})
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

fn make_client(transport: FakeTransport) -> Client {
    let mut secrets = Secrets::new();
    secrets.register("test-token");
    let config = Config {
        mcp_url: "http://ha.test:8123/api/webhook/test_secret".to_string(),
        mcp_auth: Default::default(),
        token: "test-token".to_string(),
        timeout: 5,
        connect_timeout: 5,
    };
    Client::new(config, Box::new(transport), &secrets)
}

fn parse_cli(argv: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("ha").chain(argv.iter().copied())).unwrap()
}

fn run(
    argv: &[&str],
    tools: Json,
    call_results: Vec<Json>,
    error: Option<HaCliError>,
) -> (Result<String, HaCliError>, Fake) {
    // Аналог monkeypatch.setenv("XDG_CACHE_HOME", tmp_path): изолируем кэш
    // инструментов; guard держится до конца run(), тесты сериализуются.
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path());
    std::mem::forget(dir);
    let fake = Fake::new(tools, call_results, error);
    let cli = parse_cli(argv);
    let mut client = make_client(fake.transport.clone());
    let result = dispatch(&cli, &mut client);
    (result, fake)
}

fn err_json(err: &HaCliError) -> Json {
    serde_json::from_str(&err.to_json()).unwrap()
}

// --- tools ---

#[test]
fn tools_prints_names() {
    let (result, _fake) = run(&["tools"], default_tools(), vec![], None);
    assert_eq!(
        result.unwrap(),
        "ha_get_overview\nha_search\nha_get_state\nha_call_service\n"
    );
}

#[test]
fn tools_json_output() {
    let (result, _fake) = run(
        &["tools", "--json"],
        json!([{"name": "HassTurnOn"}, {"name": "HassTurnOff"}]),
        vec![],
        None,
    );
    let out = result.unwrap();
    assert_eq!(
        serde_json::from_str::<Json>(out.trim_end()).unwrap(),
        json!([{"name": "HassTurnOn"}, {"name": "HassTurnOff"}])
    );
}

#[test]
fn error_json_on_stderr_no_traceback() {
    let (result, _fake) = run(
        &["tools"],
        default_tools(),
        vec![],
        Some(HaCliError::new(
            ErrorType::Connection,
            "Unable to connect to Home Assistant",
        )),
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONNECTION_ERROR);
    assert_eq!(
        err_json(&err),
        json!({
            "ok": false,
            "error": {
                "type": "connection_error",
                "message": "Unable to connect to Home Assistant",
            },
        })
    );
}

#[test]
fn auth_error_exit_code() {
    let (result, _fake) = run(
        &["tools"],
        default_tools(),
        vec![],
        Some(HaCliError::new(ErrorType::Authentication, "bad token")),
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_AUTHENTICATION_ERROR);
    assert_eq!(err_json(&err)["error"]["type"], "authentication_error");
}

// --- intent ---

#[test]
fn intent_success_prints_json_stdout() {
    let (result, fake) = run(
        &[
            "intent",
            "HassTurnOn",
            r#"{"area": "Кухня", "domain": "light"}"#,
        ],
        default_tools(),
        vec![
            overview(json!({"light": 1})),
            search_page(json!([entity("light.kitchen", "Kitchen Light", "Кухня")])),
            json!({"structuredContent": {"success": true}}),
        ],
        None,
    );
    let out = result.unwrap();
    let parsed: Json = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(parsed["ok"], json!(true));
    assert_eq!(parsed["response_type"], json!("action_done"));
    assert!(parsed["speech"].as_str().unwrap().contains("Kitchen Light"));
    // Полный путь ha-mcp: обзор → поиск → один вызов сервиса.
    let calls = fake.calls();
    let names: Vec<&str> = calls.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["ha_get_overview", "ha_search", "ha_call_service"]
    );
    let service = &calls[2].1;
    assert_eq!(service["domain"], json!("light"));
    assert_eq!(service["service"], json!("turn_on"));
    assert_eq!(service["entity_id"], json!("light.kitchen"));
}

#[test]
fn intent_blocked_intent_exit_2() {
    let (result, fake) = run(
        &["intent", "HassBroadcast", "{}"],
        default_tools(),
        vec![],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_INVALID_ARGUMENTS);
    assert_eq!(EXIT_INVALID_ARGUMENTS, 2);
    assert_eq!(err_json(&err)["error"]["type"], "invalid_arguments");
    assert!(fake.calls().is_empty());
}

#[test]
fn intent_invalid_json_exit_2() {
    let (result, fake) = run(
        &["intent", "HassTurnOn", "{not json"],
        default_tools(),
        vec![],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_INVALID_ARGUMENTS);
    assert_eq!(err.message, "payload is not valid JSON: '{not json'");
    assert_eq!(err_json(&err)["error"]["type"], "invalid_arguments");
    assert!(fake.calls().is_empty());
}

#[test]
fn intent_payload_not_object_exit_2() {
    let (result, fake) = run(
        &["intent", "HassTurnOn", "[1, 2]"],
        default_tools(),
        vec![],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_INVALID_ARGUMENTS);
    assert_eq!(err.message, "payload must be a JSON object");
    assert!(fake.calls().is_empty());
}

#[test]
fn intent_payload_string_exit_2() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", "\"turn on\""],
        default_tools(),
        vec![],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_INVALID_ARGUMENTS);
}

#[test]
fn intent_service_is_error_exit_7() {
    let (result, _fake) = run(
        &[
            "intent",
            "HassTurnOn",
            r#"{"area": "Кухня", "domain": "light"}"#,
        ],
        default_tools(),
        vec![
            overview(json!({"light": 1})),
            search_page(json!([entity("light.kitchen", "Kitchen Light", "Кухня")])),
            json!({
                "content": [{"type": "text", "text": "target refused the command"}],
                "isError": true,
            }),
        ],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_INTENT_EXECUTION_ERROR);
    assert_eq!(EXIT_INTENT_EXECUTION_ERROR, 7);
    let data = err_json(&err);
    assert_eq!(data["error"]["type"], "intent_failed");
    assert!(data["error"]["message"]
        .as_str()
        .unwrap()
        .contains("target refused the command"));
}

#[test]
fn intent_missing_catalog_tool_exit_8() {
    // ha_search отсутствует в tools/list: разрешение цели невозможно.
    let (result, _fake) = run(
        &["intent", "HassTurnOn", r#"{"area": "Кухня"}"#],
        json!([{"name": "ha_get_overview"}, {"name": "ha_call_service"}]),
        vec![overview(json!({"light": 1}))],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_TOOL_NOT_FOUND);
    assert_eq!(EXIT_TOOL_NOT_FOUND, 8);
    assert_eq!(err_json(&err)["error"]["type"], "tool_not_found");
}

#[test]
fn intent_connection_error_stderr_json() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", r#"{"area": "Кухня"}"#],
        default_tools(),
        vec![],
        Some(HaCliError::new(
            ErrorType::Connection,
            "Unable to connect to Home Assistant",
        )),
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONNECTION_ERROR);
    assert_eq!(err_json(&err)["error"]["type"], "connection_error");
}

#[test]
fn intent_no_stdout_on_error() {
    let (result, _fake) = run(
        &[
            "intent",
            "HassTurnOn",
            r#"{"area": "Кухня", "domain": "light"}"#,
        ],
        default_tools(),
        vec![
            overview(json!({"light": 1})),
            search_page(json!([entity("light.kitchen", "Kitchen Light", "Кухня")])),
            json!({
                "content": [{"type": "text", "text": "boom"}],
                "isError": true,
            }),
        ],
        None,
    );
    assert!(result.is_err());
}

// --- context ---

fn context_tools() -> Json {
    json!([{"name": "ha_get_overview"}, {"name": "ha_search"}])
}

fn context_results() -> Vec<Json> {
    vec![
        overview(json!({"light": 1})),
        search_page(json!([entity("light.kitchen", "Kitchen Light", "Kitchen")])),
    ]
}

#[test]
fn context_success_prints_parsed_json_stdout() {
    let (result, fake) = run(&["context"], context_tools(), context_results(), None);
    let out = result.unwrap();
    assert_eq!(
        serde_json::from_str::<Json>(out.trim_end()).unwrap(),
        json!({"ok": true, "areas": {"Kitchen": {"light": ["Kitchen Light"]}}})
    );
    // Порядок ключей: ok, areas.
    assert!(out.starts_with("{\"ok\": true, \"areas\":"));
    let calls = fake.calls();
    assert_eq!(calls[0].0, "ha_get_overview");
    assert_eq!(calls[0].1["fields"], json!(["domain_stats"]));
    assert_eq!(calls[1].0, "ha_search");
    assert_eq!(calls[1].1["domain_filter"], json!("light"));
}

#[test]
fn context_raw_prints_aggregated_catalog() {
    let (result, _fake) = run(
        &["context", "--raw"],
        context_tools(),
        context_results(),
        None,
    );
    let out = result.unwrap();
    let parsed: Json = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(parsed["entities"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["entity_total_matches"], json!(1));
    assert_eq!(parsed["partial"], json!(false));
    assert_eq!(parsed["source"], json!("ha_search"));
}

#[test]
fn context_malformed_response_is_error_exit_10() {
    let (result, _fake) = run(
        &["context"],
        context_tools(),
        vec![
            overview(json!({"light": 1})),
            // Страница ha_search с partial: каталог неполон → context_failed.
            json!({"structuredContent": {
                "success": true,
                "entities": [],
                "entity_total_matches": 0,
                "partial": true,
                "errors": [],
                "entity_has_more": false,
                "entity_next_offset": null,
            }}),
        ],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONTEXT_ERROR);
    assert_eq!(EXIT_CONTEXT_ERROR, 10);
    assert_eq!(err_json(&err)["error"]["type"], "context_failed");
}

#[test]
fn context_is_error_exit_10() {
    let (result, _fake) = run(
        &["context"],
        context_tools(),
        vec![json!({
            "content": [{"type": "text", "text": "overview unavailable"}],
            "isError": true,
        })],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONTEXT_ERROR);
    let data = err_json(&err);
    assert_eq!(data["error"]["type"], "context_failed");
    assert!(data["error"]["message"]
        .as_str()
        .unwrap()
        .contains("overview unavailable"));
}

#[test]
fn context_tool_missing_exit_8() {
    let (result, _fake) = run(
        &["context"],
        json!([{"name": "ha_call_service"}]),
        vec![],
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_TOOL_NOT_FOUND);
    assert_eq!(err_json(&err)["error"]["type"], "tool_not_found");
}

#[test]
fn context_connection_error_stderr_no_stdout() {
    let (result, _fake) = run(
        &["context"],
        context_tools(),
        vec![],
        Some(HaCliError::new(
            ErrorType::Connection,
            "Unable to connect to Home Assistant",
        )),
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONNECTION_ERROR);
    assert_eq!(err_json(&err)["error"]["type"], "connection_error");
}

#[test]
fn context_compact_prints_flat_compact_json() {
    let (result, _fake) = run(
        &["context", "--compact"],
        context_tools(),
        context_results(),
        None,
    );
    let out = result.unwrap();
    assert_eq!(
        serde_json::from_str::<Json>(out.trim_end()).unwrap(),
        json!({"ok": true, "Kitchen": {"light": ["Kitchen Light"]}})
    );
    assert!(out.starts_with("{\"ok\": true, \"Kitchen\":"));
}

// --- config ---

#[test]
fn config_error_exit_code() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("HA_TOKEN");
    std::env::remove_var("HA_TOKEN_FILE");
    std::env::remove_var("HA_MCP_URL");
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let mut secrets = Secrets::new();
    let err = ha_cli::config::load_config(&mut secrets).unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONFIGURATION_ERROR);
    assert_eq!(err_json(&err)["error"]["type"], "configuration_error");
}

// --- redaction ---

#[test]
fn error_output_redacts_token() {
    let err = HaCliError::new(ErrorType::Connection, "refused for test-token connection");
    let mut secrets = Secrets::new();
    secrets.register("test-token");
    let redacted = secrets.redact(&err.to_json());
    assert!(!redacted.contains("test-token"));
    assert!(redacted.contains("[REDACTED]"));
}

#[test]
fn intent_tool_error_with_reflected_secrets_is_redacted_in_error_json() {
    // Сообщение ToolError, отражающее webhook URL и bearer токен, не
    // должно попасть в stderr JSON без редактирования.
    let webhook = "http://ha.local/api/webhook/wh_s3cret";
    let result = json!({
        "content": [{
            "type": "text",
            "text": format!("POST {webhook} failed: bearer ha_tok_123 invalid"),
        }],
        "isError": true,
    });
    let mut secrets = Secrets::new();
    secrets.register("ha_tok_123");
    ha_cli::config::register_mcp_secrets(&mut secrets, webhook);
    // Текст ToolError проходит через redact перед печатью в stderr.
    let rendered = secrets.redact(&format!("service call failed: {result:?}"));
    assert!(!rendered.contains("wh_s3cret"));
    assert!(!rendered.contains("/api/webhook/"));
    assert!(!rendered.contains("ha_tok_123"));
    assert!(rendered.contains("[REDACTED]"));
}
