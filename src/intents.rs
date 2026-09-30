use crate::errors::{ErrorType, HaCliError};

pub const INITIAL_INTENT_SET: &[&str] = &[
    "HassTurnOn",
    "HassTurnOff",
    "HassGetState",
    "HassLightSet",
    "HassSetPosition",
];

pub fn validate_intent(intent: &str) -> Result<(), HaCliError> {
    if INITIAL_INTENT_SET.contains(&intent) {
        return Ok(());
    }
    Err(HaCliError::new(
        ErrorType::InvalidArguments,
        format!(
            "unknown or blocked intent: {intent}; allowed: {}",
            INITIAL_INTENT_SET.join(", ")
        ),
    ))
}

// TODO(phase-3): execute(), normalize_result() — перенос ha_cli/intents.py
pub fn execute(
    _client: &crate::client::Client,
    _intent: &str,
    _payload: serde_json::Value,
) -> Result<serde_json::Value, HaCliError> {
    Err(HaCliError::new(
        ErrorType::Generic,
        "intents::execute not implemented yet",
    ))
}
