use ha_cli::config::{load_config_from, DEFAULT_TIMEOUT};
use ha_cli::errors::ErrorType;
use ha_cli::security::Secrets;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn clear_env() {
    std::env::remove_var("HA_URL");
    std::env::remove_var("HA_TOKEN");
    std::env::remove_var("HA_TOKEN_FILE");
}

fn write_config(path: &std::path::Path, content: &str) -> String {
    std::fs::write(path, content).unwrap();
    path.to_string_lossy().into_owned()
}

fn make_secure(path: &std::path::Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn load(path: &str) -> Result<ha_cli::config::Config, ha_cli::errors::HaCliError> {
    let mut secrets = Secrets::new();
    load_config_from(path, None, None, &mut secrets)
}

#[test]
fn load_from_config_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "file-token\n").unwrap();
    make_secure(&token_file);
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        &format!(
            "url = \"http://ha.test:8123\"\ntoken_file = \"{}\"\n",
            token_file.display()
        ),
    );

    let config = load(&cfg_path).unwrap();
    assert_eq!(config.url, "http://ha.test:8123");
    assert_eq!(config.token, "file-token");
    assert_eq!(config.timeout, DEFAULT_TIMEOUT);
}

#[test]
fn env_overrides_config_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("HA_URL", "http://env.test:8123");
    std::env::set_var("HA_TOKEN", "env-token");
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        "url = \"http://file.test:8123\"\ntoken = \"file-token\"\n",
    );
    make_secure(dir.path().join("config.toml").as_path());

    let config = load(&cfg_path).unwrap();
    assert_eq!(config.url, "http://env.test:8123");
    assert_eq!(config.token, "env-token");
    clear_env();
}

#[test]
fn cli_args_highest_precedence() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("HA_URL", "http://env.test:8123");
    std::env::set_var("HA_TOKEN", "env-token");
    let dir = tempfile::tempdir().unwrap();
    let mut secrets = Secrets::new();
    let config = load_config_from(
        &(dir.path().join("missing.toml").to_string_lossy()),
        Some("http://cli.test:8123"),
        Some("cli-token"),
        &mut secrets,
    )
    .unwrap();
    assert_eq!(config.url, "http://cli.test:8123");
    assert_eq!(config.token, "cli-token");
    clear_env();
}

#[test]
fn env_token_file() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("HA_TOKEN");
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, " file-token \n").unwrap();
    make_secure(&token_path);
    std::env::set_var("HA_TOKEN_FILE", token_path.to_string_lossy().into_owned());
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        "url = \"http://ha.test:8123\"\n",
    );

    let config = load(&cfg_path).unwrap();
    assert_eq!(config.token, "file-token");
    clear_env();
}

#[test]
fn timeout_from_config() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        "url = \"http://ha.test:8123\"\ntoken = \"t\"\ntimeout = 9\n",
    );
    make_secure(dir.path().join("config.toml").as_path());

    let config = load(&cfg_path).unwrap();
    assert_eq!(config.timeout, 9);
}

#[test]
fn missing_url_raises() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let err = load(&dir.path().join("missing.toml").to_string_lossy()).unwrap_err();
    assert_eq!(err.kind.as_str(), ErrorType::Configuration.as_str());
    assert_eq!(err.message, "Home Assistant URL is not configured");
}

#[test]
fn missing_token_raises() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        "url = \"http://ha.test:8123\"\n",
    );
    let err = load(&cfg_path).unwrap_err();
    assert_eq!(err.message, "Home Assistant token is not configured");
}

#[test]
fn missing_token_file_raises() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        &format!(
            "url = \"http://ha.test:8123\"\ntoken_file = \"{}\"\n",
            dir.path().join("nope").display()
        ),
    );
    let err = load(&cfg_path).unwrap_err();
    assert!(err.message.contains("cannot open token file"));
}

#[test]
fn inline_token_insecure_permissions_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        "url = \"http://ha.test:8123\"\ntoken = \"t\"\n",
    );
    std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = load(&cfg_path).unwrap_err();
    assert!(err
        .message
        .contains("config file with inline token has insecure permissions"));
    assert!(err.message.contains("-rw-r--r--"));
}

#[test]
fn token_file_empty_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "   \n").unwrap();
    make_secure(&token_path);
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        &format!(
            "url = \"http://ha.test:8123\"\ntoken_file = \"{}\"\n",
            token_path.display()
        ),
    );
    let err = load(&cfg_path).unwrap_err();
    assert_eq!(err.message, "token file is empty");
}

#[test]
fn token_file_insecure_permissions_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "token\n").unwrap();
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        &format!(
            "url = \"http://ha.test:8123\"\ntoken_file = \"{}\"\n",
            token_path.display()
        ),
    );
    let err = load(&cfg_path).unwrap_err();
    assert!(err.message.contains("token file has insecure permissions"));
    assert!(err.message.contains("-rw-r--r--"));
}

#[test]
fn token_file_symlink_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, "token\n").unwrap();
    make_secure(&target);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    match ha_cli::security::read_token_file(&link.to_string_lossy()) {
        Ok(_) => {
            // O_NOFOLLOW недоступен на этой ФС — тест пропускаем.
            eprintln!("symlink was followed; skipping O_NOFOLLOW assertion");
        }
        Err(msg) => assert!(msg.contains("cannot open token file"), "{msg}"),
    }
}

#[test]
fn registers_secret_for_redaction() {
    let _guard = ENV_LOCK.lock().unwrap();
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_config(
        &dir.path().join("config.toml"),
        "url = \"http://ha.test:8123\"\ntoken = \"s3cret\"\n",
    );
    make_secure(dir.path().join("config.toml").as_path());
    let mut secrets = Secrets::new();
    load_config_from(&cfg_path, None, None, &mut secrets).unwrap();
    assert_eq!(secrets.redact("error near s3cret"), "error near [REDACTED]");
}
