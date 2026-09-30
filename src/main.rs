fn main() -> std::process::ExitCode {
    match ha_cli::cli::run() {
        Ok(code) => std::process::ExitCode::from(code),
    }
}
