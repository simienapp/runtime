use super::*;
use crate::modules::http;

/// @deka/http — outbound HTTP/1.1 + HTTP/2, streaming, cookie jars,
/// WebSocket client. See `crates/deka_host/src/modules/http.rs` for
/// the full action list and the DoD in issue #128.
#[op2]
#[serde]
pub(super) fn op_deka_http_call(
    #[string] action: String,
    #[serde] args: serde_json::Value,
) -> Result<serde_json::Value, simien_core::error::CoreError> {
    Ok(http::http_call(&action, &args))
}
