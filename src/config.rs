use crate::errors::HaCliError;

pub struct Config {
    pub url: String,
    pub token: String,
    pub timeout: u64,
}

pub const DEFAULT_TIMEOUT: u64 = 5;
const DEFAULT_CONFIG_PATH: &str = "~/.config/ha-cli/config.toml";

// TODO(phase-1): load_config — перенос ha_cli/config.py:
//  - чтение TOML (crate toml), env HA_URL / HA_TOKEN / HA_TOKEN_FILE,
//    приоритет: CLI arg > env > token_file > inline token
//  - проверка прав 0600 на файле конфига с inline token
//    и на token file (security::require_secure_mode)
//  - expanduser через crate dirs / ручную замену ~
pub fn load_config() -> Result<Config, HaCliError> {
    let _ = DEFAULT_CONFIG_PATH;
    Err(HaCliError::new(
        crate::errors::ErrorType::Configuration,
        "config::load_config not implemented yet",
    ))
}
