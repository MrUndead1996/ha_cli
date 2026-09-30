mod cli;
mod client;
mod config;
mod context;
mod discovery;
mod errors;
mod intents;
mod models;
mod security;

fn main() -> std::process::ExitCode {
    match cli::run() {
        Ok(code) => std::process::ExitCode::from(code),
    }
}
