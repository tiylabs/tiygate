//! Contract tests that keep source-backed structured-output profiles aligned
//! with the core protocol compatibility rules.

use std::error::Error;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use tiygate_core::protocol::structured_output::{
    validate_schema_for_target, StructuredOutputError,
};
use tiygate_core::ProtocolSuite;

#[derive(Debug, Deserialize)]
struct StructuredOutputProfile {
    id: String,
    vendor: String,
    source: String,
    profile_status: String,
    #[serde(default)]
    known_supported_keywords: Vec<String>,
    #[serde(default)]
    known_unsupported_keywords: Vec<String>,
}

#[test]
fn source_backed_profiles_match_target_schema_rules() -> Result<(), Box<dyn Error>> {
    for filename in ["openai.toml", "anthropic.toml", "gemini.toml"] {
        let profile = load_profile(filename)?;
        assert!(profile.source.starts_with("https://"));
        assert_eq!(profile.profile_status, "bootstrap");

        let target = target_for_vendor(&profile.vendor)?;
        for keyword in &profile.known_supported_keywords {
            let result = validate_schema_for_target(
                &schema_with_keyword(keyword)?,
                &target.default_endpoint(),
            );
            assert!(
                result.is_ok(),
                "profile {} marks {keyword:?} supported for {}, got {result:?}",
                profile.id,
                target.label(),
            );
        }

        for keyword in &profile.known_unsupported_keywords {
            let result = validate_schema_for_target(
                &schema_with_keyword(keyword)?,
                &target.default_endpoint(),
            );
            assert!(
                result.is_err(),
                "profile {} marks {keyword:?} unsupported for {}",
                profile.id,
                target.label(),
            );
            let error = match result {
                Err(error) => error,
                Ok(()) => return Ok(()),
            };
            assert!(matches!(
                error,
                StructuredOutputError::UnsupportedKeyword {
                    target: ProtocolSuite::AnthropicMessages,
                    keyword: rejected_keyword,
                    ..
                } if rejected_keyword == keyword.as_str()
            ));
        }
    }

    Ok(())
}

fn load_profile(filename: &str) -> Result<StructuredOutputProfile, Box<dyn Error>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol-specs/structured-output")
        .join(filename);
    let contents = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&contents)?)
}

fn target_for_vendor(vendor: &str) -> Result<ProtocolSuite, Box<dyn Error>> {
    match vendor {
        "OpenAI" => Ok(ProtocolSuite::OpenAiCompatible),
        "Anthropic" => Ok(ProtocolSuite::AnthropicMessages),
        "Google" => Ok(ProtocolSuite::GoogleGemini),
        other => Err(format!("unknown structured-output profile vendor: {other}").into()),
    }
}

fn schema_with_keyword(keyword: &str) -> Result<Value, Box<dyn Error>> {
    let value = match keyword {
        "title" | "description" | "format" => json!("example"),
        "properties" => json!({"nested": {"type": "string"}}),
        "required" => json!(["nested"]),
        "additionalProperties" => json!(false),
        "items" => json!({"type": "string"}),
        "prefixItems" => json!([{"type": "string"}]),
        "enum" => json!(["example"]),
        "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf" => {
            json!(1)
        }
        "minItems" | "maxItems" | "minLength" | "maxLength" => json!(1),
        other => return Err(format!("no test fixture value for schema keyword: {other}").into()),
    };

    Ok(json!({
        "type": "object",
        "properties": {
            "value": {
                "type": "string",
                keyword: value,
            }
        }
    }))
}
