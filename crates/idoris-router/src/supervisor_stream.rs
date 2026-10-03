//! Validation for the buffered Supervisor chat path.
//!
//! A `RuntimeAdapter` currently returns a complete `ChatResponse`; it cannot
//! honor an OpenAI streaming request. The HTTP layer calls this before any
//! budget reservation or Supervisor operation, so unsupported streaming is
//! an explicit client error with no model side effects.

use serde_json::{Map, Value};

/// Reject stream values that the buffered Supervisor path cannot honor.
///
/// `false` and an omitted key preserve the ordinary buffered response. A
/// present `null` or a value of another JSON type is rejected instead of
/// silently interpreted as `false`.
pub(crate) fn validate(object: &Map<String, Value>) -> Result<(), &'static str> {
    match object.get("stream") {
        None | Some(Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) => Err(
            "stream=true is unsupported for on_demand/Supervisor models; retry with stream=false",
        ),
        Some(_) => {
            Err("stream must be a boolean; on_demand/Supervisor models support only stream=false")
        }
    }
}
