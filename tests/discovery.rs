use filetime::{set_file_mtime, FileTime};
use ha_cli::client::{Client, HttpResponse, Transport};
use ha_cli::config::Config;
use ha_cli::discovery::{
    default_cache_path, default_cache_path_for, discover_tools, endpoint_cache_id, get_tool,
    is_stale_tool_result, load_cache, save_cache, ToolDiscovery, ASSIST_ENDPOINT_ID,
};
use ha_cli::errors::{ErrorType, HaCliError};
use ha_cli::security::Secrets;
use serde_json::{json, Value as Json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ---------- Mock-транспорт (как в tests/client.rs) ----------

#[derive(Clone)]
struct MockTransport {
    handler: Arc<Handler>,
    tools_list_calls: Arc<Mutex<u32>>,
}

type Resp = (u16, Vec<(String, String)>, String);
type Handler = dyn Fn(&Json, &[(String, String)]) -> Resp + Send + Sync;

impl MockTransport {
    fn new(handler: impl Fn(&Json, &[(String, String)]) -> Resp + Send + Sync + 'static) -> Self {
        Self {
            handler: Arc::new(handler),
            tools_list_calls: Arc::new(Mutex::new(0)),
        }
    }

    fn calls(&self) -> u32 {
        *self.tools_list_calls.lock().unwrap()
    }
}

impl Transport for MockTransport {
    fn post(
        &mut self,
        _url: &str,
        payload: &Json,
        _headers: &[(String, String)],
    ) -> Result<HttpResponse, HaCliError> {
        if payload["method"].as_str() == Some("tools/list") {
            *self.tools_list_calls.lock().unwrap() += 1;
        }
        let (status, headers, body) = (self.handler)(payload, _headers);
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

fn rpc_result(result: &Json, id: &Json) -> Resp {
    (
        200,
        Vec::new(),
        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    )
}

/// Клиент с последовательностью ответов tools/list (аналог FakeClient).
fn fake_client(responses: Vec<Json>) -> (Client, MockTransport) {
    let responses = std::sync::Arc::new(Mutex::new(responses.into_iter()));
    let transport = {
        let responses = responses.clone();
        MockTransport::new(
            move |payload, _headers| match payload["method"].as_str().unwrap() {
                "initialize" => {
                    rpc_result(&json!({"protocolVersion": "2025-03-26"}), &payload["id"])
                }
                "notifications/initialized" => (202, Vec::new(), String::new()),
                _ => {
                    let mut iter = responses.lock().unwrap();
                    let tools = iter.next().unwrap_or(json!([]));
                    rpc_result(&json!({"tools": tools}), &payload["id"])
                }
            },
        )
    };
    let secrets = Secrets::new();
    let config = Config {
        url: Some("http://ha.test:8123".to_string()),
        mcp_url: None,
        mcp_auth: Default::default(),
        token: String::new(),
        timeout: 5,
        connect_timeout: 5,
    };
    let client = Client::new(config, Box::new(transport.clone()), &secrets);
    (client, transport)
}

// ---------- discover_tools / get_tool ----------

#[test]
fn basename_strips_namespace() {
    let tools = discover_tools(vec![json!({"name": "homeassistant__HassTurnOn"})]);
    assert_eq!(
        tools.tools["HassTurnOn"].mcp_name,
        "homeassistant__HassTurnOn"
    );
}

#[test]
fn no_namespace() {
    let tools = discover_tools(vec![json!({"name": "HassTurnOn"})]);
    assert_eq!(tools.tools["HassTurnOn"].mcp_name, "HassTurnOn");
}

#[test]
fn get_tool_found() {
    let tools = discover_tools(vec![
        json!({"name": "HassTurnOn"}),
        json!({"name": "ns__HassGetState"}),
    ]);
    assert_eq!(
        get_tool(&tools, "HassGetState").unwrap().mcp_name,
        "ns__HassGetState"
    );
}

#[test]
fn required_semantic_names() {
    for (basename, mcp_name) in [
        ("HassTurnOn", "intent__HassTurnOn"),
        ("HassTurnOff", "intent__HassTurnOff"),
        ("HassGetState", "intent__HassGetState"),
        ("GetLiveContext", "homeassistant__GetLiveContext"),
    ] {
        let mapping = discover_tools(vec![json!({"name": mcp_name})]);
        assert_eq!(get_tool(&mapping, basename).unwrap().mcp_name, mcp_name);
    }
}

#[test]
fn get_tool_not_found() {
    let tools = discover_tools(vec![json!({"name": "HassTurnOn"})]);
    let err = get_tool(&tools, "HassNoSuchTool").unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::ToolNotFound.as_str());
    assert_eq!(err.message, "tool not found: HassNoSuchTool");
}

#[test]
fn ambiguous_tool_detected() {
    let tools = discover_tools(vec![
        json!({"name": "a__HassTurnOn"}),
        json!({"name": "b__HassTurnOn"}),
    ]);
    let err = get_tool(&tools, "HassTurnOn").unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::AmbiguousTool.as_str());
    assert_eq!(err.message, "ambiguous tool: HassTurnOn");
}

#[test]
fn input_schema_defaults_to_empty_object() {
    let tools = discover_tools(vec![
        json!({"name": "ns__A", "inputSchema": {"type": "object"}}),
        json!({"name": "ns__B"}),
    ]);
    assert_eq!(tools.tools["A"].input_schema, json!({"type": "object"}));
    assert_eq!(tools.tools["B"].input_schema, json!({}));
}

#[test]
fn source_tools_preserved() {
    let source = vec![json!({"name": "ns__A"})];
    let tools = discover_tools(source.clone());
    assert_eq!(tools.source_tools, source);
}

// ---------- save/load cache ----------

#[test]
fn cache_roundtrip() {
    let tools = discover_tools(vec![json!({"name": "ns__HassTurnOn"})]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    save_cache(&tools, path.to_str().unwrap(), ASSIST_ENDPOINT_ID).unwrap();
    let loaded = load_cache(path.to_str().unwrap(), ASSIST_ENDPOINT_ID).unwrap();
    assert_eq!(
        get_tool(&loaded, "HassTurnOn").unwrap().mcp_name,
        "ns__HassTurnOn"
    );
}

#[test]
fn cache_file_permissions_0600() {
    let tools = discover_tools(vec![json!({"name": "ns__A"})]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache").join("tools.json");
    save_cache(&tools, path.to_str().unwrap(), ASSIST_ENDPOINT_ID).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn load_cache_invalid_tools_field_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    std::fs::write(
        &path,
        json!({"endpoint": "assist", "tools": "nope"}).to_string(),
    )
    .unwrap();
    let err = load_cache(path.to_str().unwrap(), ASSIST_ENDPOINT_ID).unwrap_err();
    assert_eq!(err.message, "invalid tool cache");
}

// ---------- ToolDiscovery ----------

#[test]
fn discovery_uses_fresh_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache").join("tools.json");

    let (mut client, transport) = fake_client(vec![json!([{"name": "intent__HassTurnOn"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.tools(&mut client).unwrap();
    assert_eq!(transport.calls(), 1);

    let (mut client2, transport2) = fake_client(vec![]);
    let mut discovery2 = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let mapping = discovery2.tools(&mut client2).unwrap();
    assert_eq!(
        get_tool(&mapping, "HassTurnOn").unwrap().mcp_name,
        "intent__HassTurnOn"
    );
    assert_eq!(transport2.calls(), 0);
    assert!(discovery2.from_cache());
}

#[test]
fn refresh_replaces_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, transport) = fake_client(vec![
        json!([{"name": "intent__HassTurnOn"}]),
        json!([{"name": "intent__HassTurnOff"}]),
    ]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.tools(&mut client).unwrap();
    let mapping = discovery.tools_refresh(&mut client).unwrap();
    assert_eq!(
        get_tool(&mapping, "HassTurnOff").unwrap().mcp_name,
        "intent__HassTurnOff"
    );
    assert_eq!(transport.calls(), 2);
    assert!(!discovery.from_cache());
}

#[test]
fn missing_cached_tool_triggers_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, _) = fake_client(vec![json!([{"name": "intent__HassTurnOn"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.tools(&mut client).unwrap();

    let (mut client2, transport2) = fake_client(vec![json!([{"name": "intent__HassGetState"}])]);
    let mut discovery2 = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let tool = discovery2.get_tool(&mut client2, "HassGetState").unwrap();

    assert_eq!(tool.mcp_name, "intent__HassGetState");
    assert_eq!(transport2.calls(), 1);
}

#[test]
fn missing_tool_after_refresh_raises() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, transport2) = fake_client(vec![json!([{"name": "intent__HassTurnOn"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let err = discovery.get_tool(&mut client, "NoSuch").unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::ToolNotFound.as_str());
    assert_eq!(transport2.calls(), 1);
}

#[test]
fn invalid_cache_is_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    std::fs::write(&path, "not json").unwrap();
    let (mut client, transport) =
        fake_client(vec![json!([{"name": "homeassistant__GetLiveContext"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let mapping = discovery.tools(&mut client).unwrap();
    assert_eq!(
        get_tool(&mapping, "GetLiveContext").unwrap().mcp_name,
        "homeassistant__GetLiveContext"
    );
    assert_eq!(transport.calls(), 1);
}

#[test]
fn ttl_expiry_forces_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, _) = fake_client(vec![json!([{"name": "intent__HassTurnOn"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.tools(&mut client).unwrap();

    // mtime 10 минут назад при TTL 300 c → кэш устарел.
    let stale = FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 600,
        0,
    );
    set_file_mtime(Path::new(&path), stale).unwrap();

    let (mut client2, transport2) = fake_client(vec![json!([{"name": "intent__HassTurnOff"}])]);
    let mut discovery2 = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let mapping = discovery2.tools(&mut client2).unwrap();
    assert_eq!(
        get_tool(&mapping, "HassTurnOff").unwrap().mcp_name,
        "intent__HassTurnOff"
    );
    assert_eq!(transport2.calls(), 1);
    assert!(!discovery2.from_cache());
}

#[test]
fn fresh_cache_within_ttl_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, _) = fake_client(vec![json!([{"name": "intent__HassTurnOn"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.tools(&mut client).unwrap();

    let (mut client2, transport2) = fake_client(vec![]);
    let mut discovery2 = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery2.tools(&mut client2).unwrap();
    assert_eq!(transport2.calls(), 0);
}

// ---------- default_cache_path ----------

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn default_cache_path_uses_xdg_cache_home() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path().to_string_lossy().into_owned());
    assert_eq!(
        default_cache_path(),
        dir.path()
            .join("ha-cli")
            .join("tools-assist.json")
            .to_string_lossy()
    );
    std::env::remove_var("XDG_CACHE_HOME");
}

#[test]
fn default_cache_path_defaults_to_home_cache() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("XDG_CACHE_HOME");
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        default_cache_path(),
        Path::new(&home)
            .join(".cache")
            .join("ha-cli")
            .join("tools-assist.json")
            .to_string_lossy()
    );
}

#[test]
fn default_cache_path_for_contains_endpoint_id_not_url() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("XDG_CACHE_HOME");
    let home = std::env::var("HOME").unwrap();
    let id = endpoint_cache_id(Some("http://ha.local/api/webhook/verysecret"));
    assert!(id.starts_with("hamcp-"));
    let path = default_cache_path_for(&id);
    assert_eq!(
        path,
        Path::new(&home)
            .join(".cache")
            .join("ha-cli")
            .join(format!("tools-{id}.json"))
            .to_string_lossy()
    );
    assert!(!path.contains("verysecret"));
    assert!(!path.contains("webhook"));
}

// ---------- изоляция кэша по endpoint (этап 5.1) ----------

const SECRET_MCP_URL: &str = "http://ha.local/api/webhook/secretwebhook42";
const OTHER_MCP_URL: &str = "http://other.local/private_othersecret77";

#[test]
fn endpoint_ids_differ_and_hide_secrets() {
    let assist = endpoint_cache_id(None);
    let mcp = endpoint_cache_id(Some(SECRET_MCP_URL));
    let other = endpoint_cache_id(Some(OTHER_MCP_URL));
    assert_eq!(assist, ASSIST_ENDPOINT_ID);
    assert_ne!(mcp, other);
    assert_ne!(mcp, assist);
    // Дайджест не раскрывает секрет и стабилен (то же соединение — тот же кэш).
    assert!(!mcp.contains("secretwebhook42"));
    assert!(!mcp.contains("ha.local"));
    assert_eq!(endpoint_cache_id(Some(SECRET_MCP_URL)), mcp);
}

/// Assist-кэш (в том числе переименованный legacy `Hass*` файл) не
/// подставляется в ha-mcp-сессию: другой endpoint — другой файл.
#[test]
fn assist_cache_is_not_used_for_hamcp() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path().to_string_lossy().into_owned());
    let assist_path = dir.path().join("tools-assist.json");
    let assist_tools = discover_tools(vec![json!({"name": "homeassistant__HassTurnOn"})]);
    save_cache(
        &assist_tools,
        assist_path.to_str().unwrap(),
        ASSIST_ENDPOINT_ID,
    )
    .unwrap();

    let (mut client, transport) = fake_client(vec![json!([{"name": "ha_search"}])]);
    let mut discovery = ToolDiscovery::for_endpoint(Some(SECRET_MCP_URL));
    assert_eq!(
        Path::new(&discovery.cache_path).file_name().unwrap(),
        Path::new(&default_cache_path_for(&discovery.endpoint_id))
            .file_name()
            .unwrap()
    );
    let mapping = discovery.tools(&mut client).unwrap();
    // Инструменты Assist не видны, инструменты ha-mcp взяты из сети.
    assert!(get_tool(&mapping, "HassTurnOn").is_err());
    assert_eq!(
        get_tool(&mapping, "ha_search").unwrap().mcp_name,
        "ha_search"
    );
    assert_eq!(transport.calls(), 1);
    std::env::remove_var("XDG_CACHE_HOME");
}

/// Два разных ha-mcp endpoint'а не делят кэш друг с другом.
#[test]
fn two_hamcp_endpoints_use_separate_caches() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("ha-cli");
    let xdg = dir.path().to_string_lossy().into_owned();
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("XDG_CACHE_HOME", xdg);

    let (mut client_a, _transport_a) = fake_client(vec![json!([{"name": "ha_search"}])]);
    let mut discovery_a = ToolDiscovery::for_endpoint(Some(SECRET_MCP_URL));
    discovery_a.cache_path = cache_dir
        .join(format!("tools-{}.json", discovery_a.endpoint_id))
        .to_string_lossy()
        .into_owned();
    discovery_a.tools(&mut client_a).unwrap();

    let (mut client_b, transport_b) = fake_client(vec![json!([{"name": "ha_get_state"}])]);
    let mut discovery_b = ToolDiscovery::for_endpoint(Some(OTHER_MCP_URL));
    discovery_b.cache_path = cache_dir
        .join(format!("tools-{}.json", discovery_b.endpoint_id))
        .to_string_lossy()
        .into_owned();
    let mapping = discovery_b.tools(&mut client_b).unwrap();
    // Кэш endpoint A не подставился: discovery_b пошёл в сеть и получил
    // только свои инструменты.
    assert_eq!(transport_b.calls(), 1);
    assert!(get_tool(&mapping, "ha_search").is_err());
    assert_eq!(
        get_tool(&mapping, "ha_get_state").unwrap().mcp_name,
        "ha_get_state"
    );

    std::env::remove_var("XDG_CACHE_HOME");
}

/// Legacy-кэш без поля `endpoint` (формат до этапа 5.1, включая файл с
/// `Hass*`) — fail-safe: не читается ни одной сессией, выполняется refresh.
#[test]
fn legacy_cache_without_endpoint_is_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools-hamcp-x.json");
    // Точный legacy-формат: {"tools": [...]} без endpoint.
    std::fs::write(
        &path,
        json!({"tools": [{"name": "homeassistant__HassTurnOn"}]}).to_string(),
    )
    .unwrap();

    let (mut client, transport) = fake_client(vec![json!([{"name": "ha_search"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let mapping = discovery.tools(&mut client).unwrap();
    assert_eq!(transport.calls(), 1);
    assert!(get_tool(&mapping, "HassTurnOn").is_err());
    assert_eq!(
        get_tool(&mapping, "ha_search").unwrap().mcp_name,
        "ha_search"
    );
    // Перезаписанный кэш теперь привязан к endpoint.
    assert!(load_cache(path.to_str().unwrap(), ASSIST_ENDPOINT_ID).is_ok());
}

/// Кэш с ЧУЖИМ endpoint ( Assist-файл, подсунутый по пути ha-mcp ) не читается.
#[test]
fn cache_with_foreign_endpoint_is_rejected() {
    let tools = discover_tools(vec![json!({"name": "homeassistant__HassTurnOn"})]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    save_cache(&tools, path.to_str().unwrap(), ASSIST_ENDPOINT_ID).unwrap();
    let err = load_cache(
        path.to_str().unwrap(),
        &endpoint_cache_id(Some(SECRET_MCP_URL)),
    )
    .unwrap_err();
    assert_eq!(err.message, "invalid tool cache");
}

/// В имени файла и содержимом кэша нет plaintext секретного URL/path.
#[test]
fn cache_path_and_content_contain_no_plaintext_url() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache").join("tools.json");
    let (mut client, _) = fake_client(vec![json!([{"name": "ha_search"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.endpoint_id = endpoint_cache_id(Some(SECRET_MCP_URL));
    discovery.tools(&mut client).unwrap();

    let name = Path::new(&discovery.cache_path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(!name.contains("secretwebhook42"));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("secretwebhook42"));
    assert!(!content.contains("ha.local"));
    assert!(content.contains(&format!("\"endpoint\":\"{}\"", discovery.endpoint_id)));
}

/// Отключённый сервером инструмент (disabled tools / tool search): кэш
/// может быть устаревшим, но `get_tool` выполняет refresh и не находит
/// инструмент после обновления каталога.
#[test]
fn disabled_tool_after_refresh_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, _) = fake_client(vec![json!([
        {"name": "ha_search"},
        {"name": "ha_call_service"},
    ])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    discovery.tools(&mut client).unwrap();

    // Кэш устаревает (TTL истёк), сервер отключил ha_call_service:
    // обновлённый tools/list его больше не отдаёт.
    let stale = FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 600,
        0,
    );
    set_file_mtime(Path::new(&path), stale).unwrap();

    let (mut client2, transport2) = fake_client(vec![json!([{"name": "ha_search"}])]);
    let mut discovery2 = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let err = discovery2
        .get_tool(&mut client2, "ha_call_service")
        .unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::ToolNotFound.as_str());
    assert_eq!(transport2.calls(), 1);
    assert!(!discovery2.from_cache());
}

/// `tools --refresh` перезаписывает кэш именно этого endpoint.
#[test]
fn refresh_writes_endpoint_bound_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tools.json");
    let (mut client, _) = fake_client(vec![json!([{"name": "ha_search"}])]);
    let mut discovery = ToolDiscovery::new(Some(path.to_string_lossy().into_owned()));
    let id = endpoint_cache_id(Some(SECRET_MCP_URL));
    discovery.endpoint_id = id.clone();
    let mapping = discovery.tools_refresh(&mut client).unwrap();
    assert_eq!(
        get_tool(&mapping, "ha_search").unwrap().mcp_name,
        "ha_search"
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains(&format!("\"endpoint\":\"{id}\"")));
}

// ---------- is_stale_tool_result ----------

#[test]
fn detects_stale_tool_call_result() {
    let result = json!({
        "isError": true,
        "content": [
            {"type": "text", "text": "Error calling tool: Tool \"old\" not found"}
        ],
    });
    assert!(is_stale_tool_result(&result));
}

#[test]
fn not_stale_when_is_error_missing() {
    let result = json!({
        "content": [{"type": "text", "text": "Tool \"x\" not found"}],
    });
    assert!(!is_stale_tool_result(&result));
}

#[test]
fn not_stale_when_is_error_false() {
    let result = json!({
        "isError": false,
        "content": [{"type": "text", "text": "Tool \"x\" not found"}],
    });
    assert!(!is_stale_tool_result(&result));
}

#[test]
fn not_stale_without_markers() {
    let result = json!({
        "isError": true,
        "content": [{"type": "text", "text": "some other error"}],
    });
    assert!(!is_stale_tool_result(&result));
}

#[test]
fn not_stale_without_text_content() {
    let result = json!({
        "isError": true,
        "content": [{"type": "image", "data": "..."}],
    });
    assert!(!is_stale_tool_result(&result));
}

#[test]
fn only_first_text_item_is_checked() {
    let result = json!({
        "isError": true,
        "content": [
            {"type": "text", "text": "unrelated"},
            {"type": "text", "text": "Tool \"x\" not found"},
        ],
    });
    assert!(!is_stale_tool_result(&result));
}
