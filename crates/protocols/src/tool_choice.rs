//! Normalize named OpenAI tool choices at the codec boundary.

use serde_json::{json, Value};
use tiygate_core::Error;

pub(crate) fn normalize(choice: &Value) -> Result<Value, Error> {
    let Some(kind @ ("function" | "custom")) = choice.get("type").and_then(Value::as_str) else {
        return Ok(choice.clone());
    };
    let name = choice
        .get(kind)
        .and_then(|value| value.get("name"))
        .or_else(|| choice.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::Codec(format!("{kind} tool_choice requires a nonempty name")))?;
    let mut normalized = choice.clone();
    if let Some(object) = normalized.as_object_mut() {
        object.remove("name");
        object.insert(kind.to_string(), json!({"name":name}));
    }
    Ok(normalized)
}

pub(crate) fn responses(choice: &Value) -> Result<Value, Error> {
    let mut normalized = normalize(choice)?;
    let kind = normalized
        .get("type")
        .and_then(Value::as_str)
        .map(String::from);
    if let Some(kind @ ("function" | "custom")) = kind.as_deref() {
        let name = normalized[kind]["name"].clone();
        if let Some(object) = normalized.as_object_mut() {
            object.remove(kind);
            object.insert("name".into(), name);
        }
    }
    Ok(normalized)
}
