//! OpenCode provider implementations.
//!
//! OpenCode exposes multiple upstream protocols from fixed provider base URLs.
//! The concrete egress protocol is selected from the egress model id, while the
//! configured API base is kept unchanged for every suite.

use std::sync::Arc;

use tiygate_auth::bearer::BearerAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct OpenCodeZenProvider {
    metadata: ProviderMetadata,
}

pub struct OpenCodeGoProvider {
    metadata: ProviderMetadata,
}

impl Default for OpenCodeZenProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for OpenCodeGoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeZenProvider {
    pub fn new() -> Self {
        Self {
            metadata: opencode_metadata("OpenCodeZen", "https://opencode.ai/zen/v1"),
        }
    }
}

impl OpenCodeGoProvider {
    pub fn new() -> Self {
        Self {
            metadata: opencode_metadata("OpenCodeGo", "https://opencode.ai/zen/go/v1"),
        }
    }
}

impl Provider for OpenCodeZenProvider {
    fn id(&self) -> &str {
        "opencode-zen"
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn supported_protocols(&self) -> &[ProtocolEndpoint] {
        &self.metadata.protocols
    }

    fn auth(&self) -> Arc<dyn AuthApplier> {
        Arc::new(BearerAuthApplier)
    }

    fn egress_protocol_for_model(&self, model_id: &str) -> ProtocolEndpoint {
        opencode_egress_protocol_for_model(model_id)
    }
}

impl Provider for OpenCodeGoProvider {
    fn id(&self) -> &str {
        "opencode-go"
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn supported_protocols(&self) -> &[ProtocolEndpoint] {
        &self.metadata.protocols
    }

    fn auth(&self) -> Arc<dyn AuthApplier> {
        Arc::new(BearerAuthApplier)
    }

    fn egress_protocol_for_model(&self, model_id: &str) -> ProtocolEndpoint {
        opencode_egress_protocol_for_model(model_id)
    }
}

fn opencode_metadata(display_name: &str, base_url: &str) -> ProviderMetadata {
    ProviderMetadata {
        display_name: display_name.to_string(),
        base_url: base_url.to_string(),
        auth_mode: AuthMode::Bearer,
        channels: vec!["default".to_string()],
        protocols: vec![
            ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1"),
            ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
            ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "v1"),
            ProtocolEndpoint::new(ProtocolSuite::GoogleGemini, "generateContent", "v1beta"),
        ],
        defaults: serde_json::json!({}),
    }
}

/// Shared egress protocol derivation for OpenCode providers. Image
/// models route to the images-generations endpoint within the
/// OpenAI-compatible suite; all others use `suite_for_model`.
fn opencode_egress_protocol_for_model(model_id: &str) -> ProtocolEndpoint {
    let body = model_id.split(':').next().unwrap_or(model_id);
    let body = body.rsplit('/').next().unwrap_or(body);
    let body = body.to_ascii_lowercase();

    if body.contains("image") || body.contains("dall-e") {
        return ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-generations", "v1");
    }

    suite_for_model(model_id).default_endpoint()
}

/// Pick the egress protocol suite for an OpenCode target from its egress model
/// id. The model id may be of the form `maker/model:provider`, where the
/// `maker/` prefix and `:provider` suffix are both optional.
fn suite_for_model(model_id: &str) -> ProtocolSuite {
    let body = model_id.split(':').next().unwrap_or(model_id);
    let body = body.rsplit('/').next().unwrap_or(body);
    let body = body.to_ascii_lowercase();

    // Image models use the images-generations endpoint within the
    // OpenAI-compatible suite — must be checked before `gpt`.
    if body.contains("image") || body.contains("dall-e") {
        return ProtocolSuite::OpenAiCompatible;
    }

    if body.contains("gpt") {
        ProtocolSuite::OpenAiResponses
    } else if body.contains("claude") || body.contains("minimax") || body.contains("qwen") {
        ProtocolSuite::AnthropicMessages
    } else if body.contains("gemini") {
        ProtocolSuite::GoogleGemini
    } else {
        ProtocolSuite::OpenAiCompatible
    }
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(OpenCodeZenProvider::new()),
    }
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(OpenCodeGoProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_opencode_provider_metadata() {
        let zen = OpenCodeZenProvider::new();
        assert_eq!(zen.id(), "opencode-zen");
        assert_eq!(zen.metadata().display_name, "OpenCodeZen");
        assert_eq!(zen.metadata().base_url, "https://opencode.ai/zen/v1");
        assert!(matches!(zen.metadata().auth_mode, AuthMode::Bearer));
        assert_eq!(zen.metadata().channels, vec!["default"]);

        let go = OpenCodeGoProvider::new();
        assert_eq!(go.id(), "opencode-go");
        assert_eq!(go.metadata().display_name, "OpenCodeGo");
        assert_eq!(go.metadata().base_url, "https://opencode.ai/zen/go/v1");
        assert!(matches!(go.metadata().auth_mode, AuthMode::Bearer));
        assert_eq!(go.metadata().channels, vec!["default"]);
    }

    #[test]
    fn test_opencode_supported_protocols() {
        let provider = OpenCodeZenProvider::new();
        let suites: Vec<ProtocolSuite> = provider
            .supported_protocols()
            .iter()
            .map(|endpoint| endpoint.suite)
            .collect();

        assert_eq!(
            suites,
            vec![
                ProtocolSuite::OpenAiCompatible,
                ProtocolSuite::OpenAiResponses,
                ProtocolSuite::AnthropicMessages,
                ProtocolSuite::GoogleGemini,
            ]
        );
    }

    #[test]
    fn test_opencode_egress_protocol_for_model() {
        let provider = OpenCodeZenProvider::new();

        assert_eq!(
            provider.egress_protocol_for_model("gpt-5.5").suite,
            ProtocolSuite::OpenAiResponses
        );
        assert_eq!(
            provider
                .egress_protocol_for_model("anthropic/claude-sonnet-4:opencode")
                .suite,
            ProtocolSuite::AnthropicMessages
        );
        assert_eq!(
            provider.egress_protocol_for_model("minimax-text-01").suite,
            ProtocolSuite::AnthropicMessages
        );
        assert_eq!(
            provider.egress_protocol_for_model("qwen3-coder").suite,
            ProtocolSuite::AnthropicMessages
        );
        assert_eq!(
            provider
                .egress_protocol_for_model("google/gemini-2.5-flash:opencode")
                .suite,
            ProtocolSuite::GoogleGemini
        );
        assert_eq!(
            provider
                .egress_protocol_for_model("text-embedding-3-large")
                .suite,
            ProtocolSuite::OpenAiCompatible
        );
        assert_eq!(
            provider.egress_protocol_for_model("unknown-model").suite,
            ProtocolSuite::OpenAiCompatible
        );
    }

    #[test]
    fn test_opencode_egress_api_base_is_unchanged() {
        let provider = OpenCodeGoProvider::new();
        let messages = ProtocolSuite::AnthropicMessages.default_endpoint();
        let gemini = ProtocolSuite::GoogleGemini.default_endpoint();
        let compatible = ProtocolSuite::OpenAiCompatible.default_endpoint();

        assert_eq!(
            provider.egress_api_base("https://example.test/zen/go/v1", &messages),
            "https://example.test/zen/go/v1"
        );
        assert_eq!(
            provider.egress_api_base("https://example.test/zen/go/v1", &gemini),
            "https://example.test/zen/go/v1"
        );
        assert_eq!(
            provider.egress_api_base("https://example.test/zen/go/v1", &compatible),
            "https://example.test/zen/go/v1"
        );
    }
}
