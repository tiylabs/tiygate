//! Shared interpretation of function tool-call argument payloads.
//!
//! Every chat protocol carries function tool-call arguments as a JSON
//! *string*. An absent, empty or whitespace-only payload is the documented
//! wire representation of a call with no arguments — OpenAI streams the
//! first tool chunk as `{"name": ..., "arguments": ""}` and sends no further
//! fragments for a zero-argument function — so it normalizes to `{}`.
//! Non-empty payloads must be valid JSON: corrupt or truncated arguments are
//! rejected so an incomplete call can never complete as success.

use serde_json::{json, Value};
use tiygate_core::Error;

/// Parse a wire function tool-call `arguments` payload.
///
/// `label` identifies the call site in the codec error message.
pub(crate) fn parse_function_arguments(raw: &str, label: &str) -> Result<Value, Error> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|error| Error::Codec(format!("invalid {label}: {error}")))
}

/// True when a wire payload represents a call with no arguments.
pub(crate) fn is_empty_arguments(raw: &str) -> bool {
    raw.trim().is_empty()
}

/// A truncated turn can retain text/usage without exposing its unfinished
/// function call as executable. Completed turns still reject malformed JSON.
pub(crate) fn parse_response_arguments(
    raw: &str,
    label: &str,
    truncated: bool,
) -> Result<Option<Value>, Error> {
    match parse_function_arguments(raw, label) {
        Ok(arguments) => Ok(Some(arguments)),
        Err(_) if truncated => Ok(None),
        Err(error) => Err(error),
    }
}
