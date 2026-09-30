use crate::errors::HaCliError;
use serde_json::Value as Json;

pub const ASSIST_CONTEXT_TOOL: &str = "GetLiveContext";

// TODO(phase-4): перенос ha_cli/context.py:
//  - get_raw_result / get_live_context / parse_live_context
//  - build_arguments (defaults из inputSchema, required без default -> ContextError)
//  - _parse_context_entities (парсер текстового формата "Live Context:")
//  - build_context / compact_context / size_report
//  - query_state (fallback HassGetState)
pub fn get_live_context(_client: &mut crate::client::Client) -> Result<Json, HaCliError> {
    Err(HaCliError::new(
        crate::errors::ErrorType::Context,
        "context: not implemented yet",
    ))
}

// TODO(phase-4): перенос query_state из ha_cli/context.py (casefold-сопоставление
// area/domain/name по live context).
pub fn query_state(_live_context: &Json, _payload: &Json) -> Result<Json, HaCliError> {
    Err(HaCliError::new(
        crate::errors::ErrorType::Context,
        "context: not implemented yet",
    ))
}
