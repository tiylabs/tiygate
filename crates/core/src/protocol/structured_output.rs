//! Structured-output schema compatibility checks.
//!
//! This module owns the protocol-level part of structured-output conversion:
//! it validates the user-supplied JSON Schema against the target protocol's
//! documented dialect before a codec puts it on the wire. It deliberately
//! performs no I/O, no model routing, and no best-effort schema rewriting.
//!
//! The source-backed profile inventories live in `protocol-specs/`. The
//! checks below are intentionally monotonic: only constraints documented as
//! incompatible are rejected until the corresponding target profile has a
//! reviewed classification. This avoids silently changing a schema while
//! still preventing known lossy Anthropic conversions.

use std::fmt;

use serde_json::Value;

use crate::ir::ResponseFormat;
use crate::protocol::{ProtocolEndpoint, ProtocolSuite};

/// A structured-output schema cannot be represented faithfully by a target
/// protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredOutputError {
    /// JSON output schemas are object-valued documents for all supported wire
    /// carriers in this gateway.
    SchemaMustBeObject {
        /// JSON Pointer to the invalid schema node.
        pointer: String,
    },
    /// The target documents this JSON Schema keyword as unsupported.
    UnsupportedKeyword {
        /// Target protocol suite.
        target: ProtocolSuite,
        /// JSON Pointer to the unsupported keyword.
        pointer: String,
        /// JSON Schema keyword.
        keyword: String,
    },
    /// A schema-bearing keyword did not contain a schema or schema collection.
    InvalidSchemaContainer {
        /// JSON Pointer to the invalid value.
        pointer: String,
        /// Expected wire shape.
        expected: &'static str,
    },
}

impl fmt::Display for StructuredOutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SchemaMustBeObject { pointer } => {
                write!(f, "JSON Schema at {pointer} must be an object")
            }
            Self::UnsupportedKeyword {
                target,
                pointer,
                keyword,
            } => write!(
                f,
                "JSON Schema keyword {keyword:?} at {pointer} is not supported by {}",
                target.label()
            ),
            Self::InvalidSchemaContainer { pointer, expected } => {
                write!(f, "JSON Schema value at {pointer} must be {expected}")
            }
        }
    }
}

impl std::error::Error for StructuredOutputError {}

/// Validate a response-format constraint for the target protocol.
///
/// `JsonObject` needs no recursive schema validation: its protocol encoders
/// use a root `{ "type": "object" }` schema where needed. `Text` is the
/// default output mode and carries no schema.
pub fn validate_response_format_for_target(
    format: Option<&ResponseFormat>,
    target: &ProtocolEndpoint,
) -> Result<(), StructuredOutputError> {
    let Some(ResponseFormat::JsonSchema { schema, .. }) = format else {
        return Ok(());
    };

    validate_schema_for_target(schema, target)
}

/// Validate a JSON Schema document for the target protocol's output dialect.
pub fn validate_schema_for_target(
    schema: &Value,
    target: &ProtocolEndpoint,
) -> Result<(), StructuredOutputError> {
    walk_schema(schema, target.suite, "")
}

fn walk_schema(
    schema: &Value,
    target: ProtocolSuite,
    pointer: &str,
) -> Result<(), StructuredOutputError> {
    let object = schema
        .as_object()
        .ok_or_else(|| StructuredOutputError::SchemaMustBeObject {
            pointer: display_pointer(pointer),
        })?;

    for keyword in object.keys() {
        if is_known_unsupported_keyword(target, keyword) {
            return Err(StructuredOutputError::UnsupportedKeyword {
                target,
                pointer: pointer_for(pointer, keyword),
                keyword: keyword.clone(),
            });
        }
    }

    walk_schema_map(object.get("properties"), target, pointer, "properties")?;
    walk_schema_map(object.get("$defs"), target, pointer, "$defs")?;
    walk_schema_map(object.get("definitions"), target, pointer, "definitions")?;
    walk_schema_map(
        object.get("dependentSchemas"),
        target,
        pointer,
        "dependentSchemas",
    )?;

    walk_schema_value(object.get("items"), target, pointer, "items")?;
    walk_schema_value(
        object.get("additionalProperties"),
        target,
        pointer,
        "additionalProperties",
    )?;
    for keyword in ["not", "if", "then", "else", "contains", "propertyNames"] {
        walk_schema_value(object.get(keyword), target, pointer, keyword)?;
    }
    for keyword in ["anyOf", "allOf", "oneOf", "prefixItems"] {
        walk_schema_array(object.get(keyword), target, pointer, keyword)?;
    }

    Ok(())
}

fn walk_schema_map(
    value: Option<&Value>,
    target: ProtocolSuite,
    pointer: &str,
    keyword: &'static str,
) -> Result<(), StructuredOutputError> {
    let Some(value) = value else {
        return Ok(());
    };
    let map = value
        .as_object()
        .ok_or_else(|| StructuredOutputError::InvalidSchemaContainer {
            pointer: pointer_for(pointer, keyword),
            expected: "an object containing schemas",
        })?;
    for (name, schema) in map {
        walk_schema(
            schema,
            target,
            &pointer_for(&pointer_for(pointer, keyword), name),
        )?;
    }
    Ok(())
}

fn walk_schema_value(
    value: Option<&Value>,
    target: ProtocolSuite,
    pointer: &str,
    keyword: &'static str,
) -> Result<(), StructuredOutputError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_boolean() && keyword == "additionalProperties" {
        return Ok(());
    }
    walk_schema(value, target, &pointer_for(pointer, keyword))
}

fn walk_schema_array(
    value: Option<&Value>,
    target: ProtocolSuite,
    pointer: &str,
    keyword: &'static str,
) -> Result<(), StructuredOutputError> {
    let Some(value) = value else {
        return Ok(());
    };
    let values = value
        .as_array()
        .ok_or_else(|| StructuredOutputError::InvalidSchemaContainer {
            pointer: pointer_for(pointer, keyword),
            expected: "an array of schemas",
        })?;
    for (index, schema) in values.iter().enumerate() {
        walk_schema(
            schema,
            target,
            &pointer_for(&pointer_for(pointer, keyword), &index.to_string()),
        )?;
    }
    Ok(())
}

fn is_known_unsupported_keyword(target: ProtocolSuite, keyword: &str) -> bool {
    matches!(target, ProtocolSuite::AnthropicMessages)
        && matches!(
            keyword,
            "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
                | "multipleOf"
                | "minLength"
                | "maxLength"
        )
}

fn pointer_for(parent: &str, token: &str) -> String {
    let escaped = token.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn display_pointer(pointer: &str) -> String {
    if pointer.is_empty() {
        "/".to_string()
    } else {
        pointer.to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{validate_schema_for_target, StructuredOutputError};
    use crate::protocol::ProtocolSuite;

    fn endpoint(suite: ProtocolSuite) -> crate::protocol::ProtocolEndpoint {
        suite.default_endpoint()
    }

    #[test]
    fn anthropic_rejects_unsupported_nested_numeric_constraint() {
        let result = validate_schema_for_target(
            &json!({
                "type": "object",
                "properties": {"score": {"type": "number", "minimum": 0}}
            }),
            &endpoint(ProtocolSuite::AnthropicMessages),
        );
        assert!(result.is_err());
        let error = match result {
            Err(error) => error,
            Ok(()) => return,
        };
        assert_eq!(
            error,
            StructuredOutputError::UnsupportedKeyword {
                target: ProtocolSuite::AnthropicMessages,
                pointer: "/properties/score/minimum".to_string(),
                keyword: "minimum".to_string(),
            }
        );
    }

    #[test]
    fn anthropic_rejects_unsupported_constraint_inside_array_items() {
        let result = validate_schema_for_target(
            &json!({
                "type": "array",
                "items": {"type": "string", "maxLength": 4}
            }),
            &endpoint(ProtocolSuite::AnthropicMessages),
        );
        assert!(result.is_err());
        let error = match result {
            Err(error) => error,
            Ok(()) => return,
        };
        assert!(error.to_string().contains("/items/maxLength"));
    }

    #[test]
    fn anthropic_checks_schema_definitions_recursively() {
        let definitions_result = validate_schema_for_target(
            &json!({
                "type": "object",
                "$defs": {
                    "bounded_name": {"type": "string", "minLength": 1}
                }
            }),
            &endpoint(ProtocolSuite::AnthropicMessages),
        );
        assert!(definitions_result.is_err());
        let definitions_error = match definitions_result {
            Err(error) => error,
            Ok(()) => return,
        };
        assert!(definitions_error
            .to_string()
            .contains("/$defs/bounded_name/minLength"));
    }

    #[test]
    fn anthropic_checks_schema_combinators_recursively() {
        let combination_result = validate_schema_for_target(
            &json!({
                "type": "object",
                "properties": {
                    "value": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "number", "maximum": 100}
                        ]
                    }
                }
            }),
            &endpoint(ProtocolSuite::AnthropicMessages),
        );
        assert!(combination_result.is_err());
        let combination_error = match combination_result {
            Err(error) => error,
            Ok(()) => return,
        };
        assert!(combination_error
            .to_string()
            .contains("/properties/value/anyOf/1/maximum"));
    }

    #[test]
    fn object_schema_is_accepted_when_no_known_lossy_constraint_exists() {
        assert!(validate_schema_for_target(
            &json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
                "additionalProperties": false,
            }),
            &endpoint(ProtocolSuite::AnthropicMessages),
        )
        .is_ok());
    }
}
