use crate::client::{Client, HttpTransport};
use crate::config;
use crate::context;
use crate::discovery::ToolDiscovery;
use crate::errors::{ErrorType, HaCliError};
use crate::intents;
use crate::security::Secrets;
use clap::{Parser, Subcommand};
use serde_json::{Map, Value as Json};

#[derive(Parser)]
#[command(name = "ha", about = "Home Assistant Intent CLI for AI agents")]
pub struct Cli {
    /// Print debug trace to stderr on unexpected errors
    #[arg(long)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Print live Assist context
    Context {
        #[arg(long)]
        raw: bool,
        #[arg(long)]
        compact: bool,
    },
    /// List available MCP tools
    Tools {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        refresh: bool,
    },
    /// Execute a Home Assistant intent
    Intent {
        intent_name: String,
        #[arg(default_value = "{}")]
        payload: String,
    },
}

/// Перенос `main()`: разбор аргументов, конфигурация, клиент,
/// диспетчеризация и отчёт об ошибках.
pub fn run() -> Result<u8, std::convert::Infallible> {
    let cli = Cli::parse();
    let mut secrets = Secrets::new();
    let config = match config::load_config(None, None, &mut secrets) {
        Ok(config) => config,
        Err(err) => return Ok(report_error(&err, &secrets, cli.debug)),
    };
    let transport = match HttpTransport::new(&config) {
        Ok(transport) => transport,
        Err(err) => return Ok(report_error(&err, &secrets, cli.debug)),
    };
    let mut client = Client::new(config, Box::new(transport), &secrets);
    Ok(match execute_with_client(&cli, &mut client) {
        Ok(code) => code,
        Err(err) => report_error(&err, &secrets, cli.debug),
    })
}

/// Отчёт об ошибке: JSON в stderr, exit code из kind.
/// Все сообщения проходят через Secrets (токен не должен протечь).
fn report_error(err: &HaCliError, secrets: &Secrets, debug: bool) -> u8 {
    if debug {
        eprintln!("{}", secrets.redact(&debug_trace(err)));
    }
    eprintln!("{}", secrets.redact(&err.to_json()));
    err.kind.exit_code()
}

/// Облегчённый аналог traceback.format_exc: сама ошибка + цепочка source.
fn debug_trace(err: &HaCliError) -> String {
    let mut trace = format!("{err:?}");
    let mut source: Option<&dyn std::error::Error> = Some(err);
    while let Some(current) = source {
        if let Some(next) = current.source() {
            trace.push_str(&format!("\ncaused by: {next}"));
        }
        source = current.source();
    }
    trace
}

/// Точка подмены клиента в тестах: печатает stdout-вывод команды.
pub fn execute_with_client(cli: &Cli, client: &mut Client) -> Result<u8, HaCliError> {
    let output = dispatch(cli, client)?;
    print!("{output}");
    Ok(EXIT_OK)
}

const EXIT_OK: u8 = 0;

/// Перенос `run(args)` без побочных эффектов: возвращает строку для stdout.
pub fn dispatch(cli: &Cli, client: &mut Client) -> Result<String, HaCliError> {
    match &cli.command {
        Command::Intent {
            intent_name,
            payload,
        } => {
            intents::validate_intent(intent_name)?;
            let parsed: Json = match serde_json::from_str(payload) {
                Ok(value) => value,
                Err(_) => {
                    // Python: f"payload is not valid JSON: {args.payload!r}"
                    return Err(HaCliError::new(
                        ErrorType::InvalidArguments,
                        format!("payload is not valid JSON: '{payload}'"),
                    ));
                }
            };
            if !parsed.is_object() {
                return Err(HaCliError::new(
                    ErrorType::InvalidArguments,
                    "payload must be a JSON object",
                ));
            }
            let result = intents::execute(client, intent_name, &parsed)?;
            Ok(to_json_line(&result))
        }
        Command::Context { raw, compact } => {
            if *raw {
                let raw_result = context::get_raw_result(client)?;
                return Ok(to_json_line(&raw_result));
            }
            let live_context = context::get_live_context(client)?;
            let built = context::build_context(&live_context);
            if *compact {
                Ok(to_json_line(&with_ok(&context::compact_context(&built))))
            } else {
                Ok(to_json_line(&with_ok(&built)))
            }
        }
        Command::Tools { json, refresh } => {
            let mut discovery = ToolDiscovery::new(None);
            let mapping = if *refresh {
                discovery.tools_refresh(client)?
            } else {
                discovery.tools(client)?
            };
            if *json {
                return Ok(to_json_line(&Json::Array(mapping.source_tools.clone())));
            }
            let mut out = String::new();
            for tool in &mapping.source_tools {
                let name = tool.get("name").and_then(Json::as_str).unwrap_or("");
                out.push_str(name);
                out.push('\n');
            }
            Ok(out)
        }
    }
}

/// Перенос `output.output_json`: компактный JSON + '\n'
/// (serde_json по умолчанию не экранирует не-ASCII — паритет
/// с json.dump(ensure_ascii=False)).
/// Перенос `output.output_json`: Python `json.dump` с разделителями
/// (', ', ': ') + '\n' (см. crate::output).
fn to_json_line(data: &Json) -> String {
    let mut text = crate::output::dumps(data);
    text.push('\n');
    text
}

/// Аналог `{"ok": True, **data}` — порядок ключей важен для паритета.
fn with_ok(data: &Json) -> Json {
    let mut merged = Map::new();
    merged.insert("ok".to_string(), Json::Bool(true));
    if let Some(object) = data.as_object() {
        for (key, value) in object {
            merged.insert(key.clone(), value.clone());
        }
    }
    Json::Object(merged)
}
