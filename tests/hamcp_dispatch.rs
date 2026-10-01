//! Интеграционные тесты полного пути CLI на ha-mcp (docs/mcp_migration.md,
//! этап 5, подпункт 2): от разбора аргументов `Cli` через `dispatch` до
//! записывающего транспорта (RecordingTransport). Юнит-покрытие отдельных
//! модулей (client/context/intents/resolver) уже существует; здесь
//! проверяется межмодульный поток: URL/auth/SSE, каталог с пагинацией,
//! разрешённый `color_temp_kelvin` → `light.turn_on data`, read-only
//! успешный вывод, запрет entity_id до сети и отсутствие повтора записи.

use clap::Parser as _;
use ha_cli::cli::{dispatch, Cli};
use ha_cli::client::{HttpResponse, Transport};
use ha_cli::config::{Config, McpAuth};
use ha_cli::errors::HaCliError;
use ha_cli::security::Secrets;
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex, MutexGuard};

// ---------- Изоляция кэша discovery (общая с другими test-файлами) ----------

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct IsolatedCache {
    // Порядок полей важен: Drop выполняется сверху вниз, прежнее значение
    // XDG_CACHE_HOME восстанавливается, пока TempDir ещё жив.
    _previous: Option<std::ffi::OsString>,
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

impl Drop for IsolatedCache {
    fn drop(&mut self) {
        // Mutex ещё удерживается guard'ом: восстановление окружения
        // сериализовано с установкой.
        match self._previous.take() {
            Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }
}

fn isolated_cache() -> IsolatedCache {
    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("XDG_CACHE_HOME");
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path());
    IsolatedCache {
        _previous: previous,
        _guard: guard,
        _dir: dir,
    }
}

// ---------- RecordingTransport: скриптованный ha-mcp сервер ----------

const MCP_URL: &str = "http://ha.local/api/webhook/wh_test_secret";
const TOKEN: &str = "ha_tok_test_secret";

#[derive(Clone)]
struct Recorded {
    url: String,
    method: String,
    tool: Option<String>,
    arguments: Json,
    headers: Vec<(String, String)>,
}

/// Как отвечает tools/call для ha_call_service.
#[derive(Clone, Copy, PartialEq)]
enum ServiceOutcome {
    Success,
    JsonRpcError,
}

struct MockState {
    requests: Vec<Recorded>,
    service_outcome: ServiceOutcome,
}

fn catalog() -> Vec<Json> {
    vec![
        json!({
            "entity_id": "light.a",
            "friendly_name": "One",
            "domain": "light",
            "state": "off",
            "area": "Kitchen",
            "aliases": [],
        }),
        json!({
            "entity_id": "light.b",
            "friendly_name": "Two",
            "domain": "light",
            "state": "off",
            "area": "Kitchen",
            "aliases": [],
        }),
        json!({
            "entity_id": "light.c",
            "friendly_name": "Three",
            "domain": "light",
            "state": "off",
            "area": "Kitchen",
            "aliases": [],
        }),
    ]
}

/// Сервер отдаёт по 2 сущности на страницу ha_search: проверяет, что CLI
/// следует `entity_has_more` / `entity_next_offset` через dispatch.
fn page(entities: &[Json], offset: i64) -> (Vec<Json>, bool, Json) {
    // Сервер отдаёт не больше 2 сущностей на страницу независимо от
    // клиентского limit: так пагинация реально проверяется.
    const SERVER_PAGE_SIZE: usize = 2;
    let start = offset as usize;
    let end = (start + SERVER_PAGE_SIZE).min(entities.len());
    let slice = entities[start..end].to_vec();
    let has_more = end < entities.len();
    let next = if has_more {
        json!(end as i64)
    } else {
        Json::Null
    };
    (slice, has_more, next)
}

fn sse_reply(payload: &Json, result: &Json) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/event-stream".to_string())],
        body: format!(
            "event: message\ndata: {}\n\n",
            json!({"jsonrpc": "2.0", "id": payload["id"], "result": result})
        ),
    }
}

fn json_reply(payload: &Json, result: &Json) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: json!({"jsonrpc": "2.0", "id": payload["id"], "result": result}).to_string(),
    }
}

fn json_rpc_error(payload: &Json, message: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: json!({
            "jsonrpc": "2.0",
            "id": payload["id"],
            "error": {"code": -32602, "message": message},
        })
        .to_string(),
    }
}

/// Шаринг состояния между Client (владеет Box<dyn Transport>) и тестом.
#[derive(Clone)]
struct SharedMock(Arc<Mutex<MockState>>);

impl SharedMock {
    fn new(service_outcome: ServiceOutcome) -> Self {
        Self(Arc::new(Mutex::new(MockState {
            requests: Vec::new(),
            service_outcome,
        })))
    }

    fn requests(&self) -> Vec<Recorded> {
        self.0.lock().unwrap().requests.clone()
    }

    fn tools(&self, name: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "tools/call" && r.tool.as_deref() == Some(name))
            .collect()
    }

    fn record(&self, url: &str, payload: &Json, headers: &[(String, String)]) {
        let request = Recorded {
            url: url.to_string(),
            method: payload["method"].as_str().unwrap_or_default().to_string(),
            tool: payload
                .pointer("/params/name")
                .and_then(Json::as_str)
                .map(str::to_string),
            arguments: payload
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(Json::Null),
            headers: headers.to_vec(),
        };
        self.0.lock().unwrap().requests.push(request);
    }
}

impl Transport for SharedMock {
    fn post(
        &mut self,
        url: &str,
        payload: &Json,
        headers: &[(String, String)],
    ) -> Result<HttpResponse, HaCliError> {
        self.record(url, payload, headers);
        let outcome = self.0.lock().unwrap().service_outcome;
        match payload["method"].as_str().unwrap_or_default() {
            // Как живой сервер (этап 1): initialize и tools/list — SSE.
            "initialize" => Ok(sse_reply(
                payload,
                &json!({"protocolVersion": "2025-03-26"}),
            )),
            "notifications/initialized" => Ok(HttpResponse {
                status: 202,
                headers: Vec::new(),
                body: String::new(),
            }),
            "tools/list" => Ok(sse_reply(
                payload,
                &json!({"tools": [
                    {"name": "ha_get_overview", "inputSchema": {"type": "object"}},
                    {"name": "ha_search", "inputSchema": {"type": "object"}},
                    {"name": "ha_get_state", "inputSchema": {"type": "object"}},
                    {"name": "ha_call_service", "inputSchema": {"type": "object"}},
                ]}),
            )),
            "tools/call" => match payload.pointer("/params/name").and_then(Json::as_str) {
                Some("ha_get_overview") => Ok(json_reply(
                    payload,
                    &json!({"structuredContent": {"domain_stats": {"light": 3}}}),
                )),
                Some("ha_search") => {
                    let offset = payload
                        .pointer("/params/arguments/offset")
                        .and_then(Json::as_i64)
                        .unwrap_or(0);
                    let (entities, has_more, next) = page(&catalog(), offset);
                    Ok(json_reply(
                        payload,
                        &json!({"structuredContent": {
                            "entities": entities,
                            "entity_total_matches": 3,
                            "partial": false,
                            "errors": [],
                            "warnings": [],
                            "entity_has_more": has_more,
                            "entity_next_offset": next,
                        }}),
                    ))
                }
                Some("ha_get_state") => Ok(json_reply(
                    payload,
                    &json!({"structuredContent": {
                        "data": {"state": "on", "attributes": {"friendly_name": "One"}},
                        "metadata": {"entity_id": "light.a"},
                    }}),
                )),
                Some("ha_call_service") => match outcome {
                    ServiceOutcome::Success => Ok(json_reply(
                        payload,
                        &json!({"structuredContent": {"success": true}}),
                    )),
                    ServiceOutcome::JsonRpcError => {
                        Ok(json_rpc_error(payload, "Tool ha_call_service not found"))
                    }
                },
                other => panic!("unexpected tool call: {other:?}"),
            },
            other => panic!("unexpected method: {other}"),
        }
    }
}

fn make_config(mcp_auth: McpAuth) -> Config {
    Config {
        mcp_url: MCP_URL.to_string(),
        mcp_auth,
        token: TOKEN.to_string(),
        timeout: 5,
        connect_timeout: 5,
    }
}

fn make_client(mcp_auth: McpAuth, transport: SharedMock) -> ha_cli::client::Client {
    let mut secrets = Secrets::new();
    secrets.register(TOKEN);
    ha_cli::config::register_mcp_secrets(&mut secrets, MCP_URL);
    ha_cli::client::Client::new(make_config(mcp_auth), Box::new(transport), &secrets)
}

fn parse_cli(argv: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("ha").chain(argv.iter().copied())).unwrap()
}

// ---------- Тесты ----------

#[test]
fn dispatch_light_set_full_flow_mcp_url_sse_pagination_and_service_data() {
    let _cache = isolated_cache();
    let transport = SharedMock::new(ServiceOutcome::Success);
    let mut client = make_client(McpAuth::HaAuth, transport.clone());

    let cli = parse_cli(&[
        "intent",
        "HassLightSet",
        r#"{"area": "Kitchen", "name": "One", "color_temp_kelvin": 3000}"#,
    ]);
    let out = dispatch(&cli, &mut client).unwrap();

    // stdout — одна JSON-строка контракта CLI без внутренних ID.
    let parsed: Json = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(parsed["ok"], json!(true));
    assert_eq!(parsed["response_type"], json!("action_done"));
    assert!(parsed["speech"].as_str().unwrap().contains("One"));
    assert!(!out.contains("light.a"));

    // URL/auth: каждый запрос ушёл на настроенный mcp_url как есть,
    // Bearer — из ha_auth.
    let requests = transport.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(request.url, MCP_URL, "{}", request.method);
    }
    let first = requests
        .iter()
        .find(|r| r.method == "initialize")
        .expect("initialize request");
    let auth = first
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .map(|(_, v)| v.as_str());
    assert_eq!(auth, Some("Bearer ha_tok_test_secret"));
    // Токен и секретный путь не попали в вывод.
    assert!(!out.contains(TOKEN));
    assert!(!out.contains("wh_test_secret"));

    // Каталог: полный обход с пагинацией по entity_next_offset —
    // две страницы light (offset 0 и 2) при лимите сервера 2.
    let searches = transport.tools("ha_search");
    assert_eq!(searches.len(), 2, "one extra page after entity_has_more");
    assert_eq!(searches[0].arguments["domain_filter"], json!("light"));
    assert_eq!(searches[0].arguments["offset"], json!(0));
    assert_eq!(searches[1].arguments["offset"], json!(2));
    for search in &searches {
        assert_eq!(search.arguments["limit"], json!(200));
        let fields = search.arguments["result_fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f == "is_group"));
    }

    // Действие: ровно один ha_call_service с разрешённым внутренним ID
    // и data.color_temp_kelvin.
    let calls = transport.tools("ha_call_service");
    assert_eq!(calls.len(), 1, "no repeated write");
    let call = &calls[0].arguments;
    assert_eq!(call["domain"], json!("light"));
    assert_eq!(call["service"], json!("turn_on"));
    assert_eq!(call["entity_id"], json!("light.a"));
    assert_eq!(call["data"]["color_temp_kelvin"], json!(3000));
    // Только прошедшие валидацию поля.
    assert_eq!(
        call["data"].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["color_temp_kelvin"]
    );
}

#[test]
fn dispatch_get_state_read_only_success_output_and_no_service_calls() {
    let _cache = isolated_cache();
    // Режим по умолчанию (без ha_auth): токен НЕ отправляется на mcp_url.
    let transport = SharedMock::new(ServiceOutcome::Success);
    let mut client = make_client(McpAuth::None, transport.clone());

    let cli = parse_cli(&["intent", "HassGetState", r#"{"name": "One"}"#]);
    let out = dispatch(&cli, &mut client).unwrap();

    let parsed: Json = serde_json::from_str(out.trim_end()).unwrap();
    assert_eq!(parsed["ok"], json!(true));
    assert_eq!(parsed["response_type"], json!("query_answer"));
    assert_eq!(parsed["speech"], json!("One: on"));
    let states = parsed["data"]["states"].as_array().unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0]["area"], json!("Kitchen"));
    assert_eq!(states[0]["domain"], json!("light"));
    assert_eq!(states[0]["name"], json!("One"));
    assert_eq!(states[0]["state"], json!("on"));
    // Внутренний ID и секреты наружу не попадают.
    assert!(!out.contains("light.a"));
    assert!(!out.contains(TOKEN));
    assert!(!out.contains("wh_test_secret"));

    // Секретный URL авторизуется сам: ни одного Authorization-заголовка.
    for request in transport.requests() {
        assert!(
            !request.headers.iter().any(|(k, _)| k == "Authorization"),
            "{}",
            request.method
        );
    }

    // Состояние читается ha_get_state по разрешённому внутреннему ID.
    let reads = transport.tools("ha_get_state");
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].arguments["entity_id"], json!("light.a"));
    assert_eq!(reads[0].arguments["fields"], json!(["state", "attributes"]));
    assert!(transport.tools("ha_call_service").is_empty(), "read-only");
}

#[test]
fn dispatch_rejects_nested_entity_id_before_any_request() {
    let _cache = isolated_cache();
    let transport = SharedMock::new(ServiceOutcome::Success);
    let mut client = make_client(McpAuth::None, transport.clone());

    let cli = parse_cli(&[
        "intent",
        "HassLightSet",
        r#"{"area": "Kitchen", "name": "One", "nested": {"entity_id": "light.a"}}"#,
    ]);
    let err = dispatch(&cli, &mut client).unwrap_err();

    assert_eq!(err.kind.exit_code(), 2);
    let err_json: Json = serde_json::from_str(&err.to_json()).unwrap();
    assert_eq!(err_json["error"]["type"], "invalid_arguments");
    assert!(!err.to_json().contains("light.a"));
    // Ни одного сетевого вызова, включая discovery и каталог.
    assert!(
        transport.requests().is_empty(),
        "no request may precede payload validation"
    );
}

#[test]
fn dispatch_write_jsonrpc_error_is_reported_once_without_retry() {
    let _cache = isolated_cache();
    let transport = SharedMock::new(ServiceOutcome::JsonRpcError);
    let mut client = make_client(McpAuth::None, transport.clone());

    let cli = parse_cli(&[
        "intent",
        "HassTurnOn",
        r#"{"area": "Kitchen", "domain": "light"}"#,
    ]);
    let err = dispatch(&cli, &mut client).unwrap_err();

    assert_eq!(err.kind.exit_code(), 7);
    assert!(err.message.contains("may still have been performed"));
    assert!(err.message.contains("not repeated"));
    // Сообщение вычищено от внутренних ID целей.
    assert!(!err.message.contains("light.a"));
    assert!(!err.message.contains("light.b"));
    // Запись отправлена ровно один раз: JSON-RPC error не трактуется как
    // stale-схема и не порождает повторного tools/call.
    let calls = transport.tools("ha_call_service");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments["entity_id"], json!("light.a"));
}
