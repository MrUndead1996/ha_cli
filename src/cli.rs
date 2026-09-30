use clap::{Parser, Subcommand};

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

pub fn run() -> Result<u8, std::convert::Infallible> {
    let cli = Cli::parse();
    match execute(&cli) {
        Ok(code) => Ok(code),
        Err(err) => {
            eprintln!("{}", err.to_json());
            Ok(err.kind.exit_code())
        }
    }
}

fn execute(_cli: &Cli) -> Result<u8, crate::errors::HaCliError> {
    // TODO(phase-1..4): маршрутизация команд — перенос ha_cli/cli.py
    Err(crate::errors::HaCliError::new(
        crate::errors::ErrorType::Generic,
        "CLI dispatch not implemented yet",
    ))
}
