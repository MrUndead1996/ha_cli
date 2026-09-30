use serde_json::json;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_GENERIC_ERROR: u8 = 1;
pub const EXIT_INVALID_ARGUMENTS: u8 = 2;
pub const EXIT_CONFIGURATION_ERROR: u8 = 3;
pub const EXIT_AUTHENTICATION_ERROR: u8 = 4;
pub const EXIT_CONNECTION_ERROR: u8 = 5;
pub const EXIT_HA_API_ERROR: u8 = 6;
pub const EXIT_INTENT_EXECUTION_ERROR: u8 = 7;
pub const EXIT_TOOL_NOT_FOUND: u8 = 8;
pub const EXIT_AMBIGUOUS_TOOL: u8 = 9;
pub const EXIT_CONTEXT_ERROR: u8 = 10;

#[derive(Debug, Clone)]
pub enum ErrorType {
    Generic,
    InvalidArguments,
    Configuration,
    Authentication,
    Connection,
    HaApi,
    Intent,
    ToolNotFound,
    AmbiguousTool,
    Context,
}

impl ErrorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generic => "generic_error",
            Self::InvalidArguments => "invalid_arguments",
            Self::Configuration => "configuration_error",
            Self::Authentication => "authentication_error",
            Self::Connection => "connection_error",
            Self::HaApi => "ha_api_error",
            Self::Intent => "intent_failed",
            Self::ToolNotFound => "tool_not_found",
            Self::AmbiguousTool => "ambiguous_tool",
            Self::Context => "context_failed",
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Generic => EXIT_GENERIC_ERROR,
            Self::InvalidArguments => EXIT_INVALID_ARGUMENTS,
            Self::Configuration => EXIT_CONFIGURATION_ERROR,
            Self::Authentication => EXIT_AUTHENTICATION_ERROR,
            Self::Connection => EXIT_CONNECTION_ERROR,
            Self::HaApi => EXIT_HA_API_ERROR,
            Self::Intent => EXIT_INTENT_EXECUTION_ERROR,
            Self::ToolNotFound => EXIT_TOOL_NOT_FOUND,
            Self::AmbiguousTool => EXIT_AMBIGUOUS_TOOL,
            Self::Context => EXIT_CONTEXT_ERROR,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HaCliError {
    pub kind: ErrorType,
    pub message: String,
}

impl HaCliError {
    pub fn new(kind: ErrorType, message: impl Into<String>) -> Self {
        // TODO(phase-0): redact message via crate::security::redact
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> String {
        json!({
            "ok": false,
            "error": {
                "type": self.kind.as_str(),
                "message": self.message,
            },
        })
        .to_string()
    }
}

impl std::fmt::Display for HaCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HaCliError {}
