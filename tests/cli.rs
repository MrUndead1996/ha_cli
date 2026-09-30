use clap::Parser as _;
use ha_cli::cli::{dispatch, Cli};
use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::errors::{
    ErrorType, HaCliError, EXIT_AMBIGUOUS_TOOL, EXIT_AUTHENTICATION_ERROR,
    EXIT_CONFIGURATION_ERROR, EXIT_CONNECTION_ERROR, EXIT_CONTEXT_ERROR,
    EXIT_INTENT_EXECUTION_ERROR, EXIT_INVALID_ARGUMENTS, EXIT_TOOL_NOT_FOUND,
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
    fn new(tools: Json, call_result: Json, error: Option<HaCliError>) -> Self {
        let calls: Arc<Mutex<Vec<(String, Json)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_handler = Arc::clone(&calls);
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
                    Ok(rpc_result(&call_result, payload["id"].as_u64().unwrap()))
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
    json!([{"name": "HassTurnOn"}, {"name": "HassTurnOff"}])
}

fn make_client(transport: FakeTransport) -> Client {
    let mut secrets = Secrets::new();
    secrets.register("test-token");
    let config = Config {
        url: "http://ha.test:8123".to_string(),
        token: "test-token".to_string(),
        timeout: 5,
    };
    Client::new(config, Box::new(transport), &secrets)
}

fn parse_cli(argv: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("ha").chain(argv.iter().copied())).unwrap()
}

fn run(
    argv: &[&str],
    tools: Json,
    call_result: Json,
    error: Option<HaCliError>,
) -> (Result<String, HaCliError>, Fake) {
    // Аналог monkeypatch.setenv("XDG_CACHE_HOME", tmp_path): изолируем кэш
    // инструментов; guard держится до конца run(), тесты сериализуются.
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path());
    std::mem::forget(dir);
    let fake = Fake::new(tools, call_result, error);
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
    let (result, _fake) = run(&["tools"], default_tools(), json!(null), None);
    assert_eq!(result.unwrap(), "HassTurnOn\nHassTurnOff\n");
}

#[test]
fn tools_json_output() {
    let (result, _fake) = run(
        &["tools", "--json"],
        json!([{"name": "HassTurnOn"}, {"name": "HassTurnOff"}]),
        json!(null),
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
        json!(null),
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
        json!(null),
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
        json!({
            "content": [{"type": "text", "text": "Turned on"}],
            "isError": false,
        }),
        None,
    );
    let out = result.unwrap();
    assert_eq!(
        out,
        "{\"ok\":true,\"response_type\":\"action_done\",\"speech\":\"Turned on\"}\n"
    );
    assert_eq!(
        fake.calls(),
        vec![(
            "HassTurnOn".to_string(),
            json!({"area": "Кухня", "domain": "light"}),
        )]
    );
}

#[test]
fn intent_blocked_intent_exit_2() {
    let (result, fake) = run(
        &["intent", "HassBroadcast", "{}"],
        default_tools(),
        json!({"content": [], "isError": false}),
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
        json!(null),
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
        json!(null),
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
        json!(null),
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_INVALID_ARGUMENTS);
}

#[test]
fn intent_tool_missing_exit_8() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", "{}"],
        json!([{"name": "HassTurnOff"}]),
        json!({"content": [], "isError": false}),
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_TOOL_NOT_FOUND);
    assert_eq!(EXIT_TOOL_NOT_FOUND, 8);
    assert_eq!(err_json(&err)["error"]["type"], "tool_not_found");
}

#[test]
fn intent_ambiguous_exit_9() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", "{}"],
        json!([{"name": "s1__HassTurnOn"}, {"name": "s2__HassTurnOn"}]),
        json!({"content": [], "isError": false}),
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_AMBIGUOUS_TOOL);
    assert_eq!(EXIT_AMBIGUOUS_TOOL, 9);
    assert_eq!(err_json(&err)["error"]["type"], "ambiguous_tool");
}

#[test]
fn intent_is_error_exit_7() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", r#"{"area": "Кухня"}"#],
        default_tools(),
        json!({
            "content": [{"type": "text", "text": "target not found"}],
            "isError": true,
        }),
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
        .contains("target not found"));
}

#[test]
fn intent_connection_error_stderr_json() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", "{}"],
        default_tools(),
        json!(null),
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
fn intent_default_payload_empty_object() {
    let (result, fake) = run(
        &["intent", "HassTurnOn"],
        default_tools(),
        json!({"content": [], "isError": false}),
        None,
    );
    let out = result.unwrap();
    assert_eq!(
        out,
        "{\"ok\":true,\"response_type\":\"action_done\",\"speech\":\"\"}\n"
    );
    assert_eq!(fake.calls(), vec![("HassTurnOn".to_string(), json!({}))]);
}

#[test]
fn intent_no_stdout_on_error() {
    let (result, _fake) = run(
        &["intent", "HassTurnOn", "{}"],
        default_tools(),
        json!({
            "content": [{"type": "text", "text": "boom"}],
            "isError": true,
        }),
        None,
    );
    assert!(result.is_err());
}

// --- context ---

fn context_tools() -> Json {
    json!([{"name": "GetLiveContext"}])
}

#[test]
fn context_success_prints_parsed_json_stdout() {
    let (result, fake) = run(
        &["context"],
        context_tools(),
        json!({
            "content": [{
                "type": "text",
                "text": "{\"entities\": [{\"entity_id\": \"light.kitchen\", \"name\": \"Kitchen Light\", \"domain\": \"light\", \"area\": \"Kitchen\"}]}",
            }],
            "isError": false,
        }),
        None,
    );
    let out = result.unwrap();
    assert_eq!(
        serde_json::from_str::<Json>(out.trim_end()).unwrap(),
        json!({"ok": true, "areas": {"Kitchen": {"light": ["Kitchen Light"]}}})
    );
    // Порядок ключей: ok, areas.
    assert!(out.starts_with("{\"ok\":true,\"areas\":"));
    assert_eq!(
        fake.calls(),
        vec![("GetLiveContext".to_string(), json!({}))]
    );
}

#[test]
fn context_raw_prints_call_tool_result() {
    let raw = json!({
        "content": [{"type": "text", "text": "not json at all"}],
        "isError": false,
    });
    let (result, fake) = run(&["context", "--raw"], context_tools(), raw.clone(), None);
    let out = result.unwrap();
    assert_eq!(serde_json::from_str::<Json>(out.trim_end()).unwrap(), raw);
    assert_eq!(
        fake.calls(),
        vec![("GetLiveContext".to_string(), json!({}))]
    );
}

#[test]
fn context_malformed_response_is_error_exit_10() {
    let (result, _fake) = run(
        &["context"],
        context_tools(),
        json!({
            "content": [{"type": "text", "text": "oops, not JSON"}],
            "isError": false,
        }),
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
        json!({
            "content": [{"type": "text", "text": "conversation agent unavailable"}],
            "isError": true,
        }),
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_CONTEXT_ERROR);
    let data = err_json(&err);
    assert_eq!(data["error"]["type"], "context_failed");
    assert!(data["error"]["message"]
        .as_str()
        .unwrap()
        .contains("conversation agent unavailable"));
}

#[test]
fn context_tool_missing_exit_8() {
    let (result, _fake) = run(
        &["context"],
        json!([{"name": "HassTurnOn"}]),
        json!(null),
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_TOOL_NOT_FOUND);
    assert_eq!(err_json(&err)["error"]["type"], "tool_not_found");
}

#[test]
fn context_ambiguous_exit_9() {
    let (result, _fake) = run(
        &["context"],
        json!([{"name": "s1__GetLiveContext"}, {"name": "s2__GetLiveContext"}]),
        json!(null),
        None,
    );
    let err = result.unwrap_err();
    assert_eq!(err.kind.exit_code(), EXIT_AMBIGUOUS_TOOL);
    assert_eq!(err_json(&err)["error"]["type"], "ambiguous_tool");
}

#[test]
fn context_connection_error_stderr_no_stdout() {
    let (result, _fake) = run(
        &["context"],
        context_tools(),
        json!(null),
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
        json!({
            "content": [{
                "type": "text",
                "text": "{\"entities\": [{\"name\": \"Kitchen Light\", \"domain\": \"light\", \"area\": \"Kitchen\", \"state\": \"on\", \"capabilities\": {\"device_class\": \"light\"}}]}",
            }],
            "isError": false,
        }),
        None,
    );
    let out = result.unwrap();
    assert_eq!(
        serde_json::from_str::<Json>(out.trim_end()).unwrap(),
        json!({"ok": true, "Kitchen": {"light": ["Kitchen Light"]}})
    );
    assert!(out.starts_with("{\"ok\":true,\"Kitchen\":"));
}

// --- config ---

#[test]
fn config_error_exit_code() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("HA_URL");
    std::env::remove_var("HA_TOKEN");
    std::env::remove_var("HA_TOKEN_FILE");
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", dir.path());
    let mut secrets = Secrets::new();
    let err = ha_cli::config::load_config(None, None, &mut secrets).unwrap_err();
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
