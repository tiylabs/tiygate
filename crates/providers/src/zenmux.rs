//! ZenMux provider implementation.
//!
//! ZenMux is an aggregation platform that exposes multiple upstream
//! protocols behind a single base URL. The concrete egress protocol and
//! suite-specific base URL are selected by this provider from the egress
//! model id.

use std::sync::Arc;

use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct ZenMuxProvider {
    metadata: ProviderMetadata,
}

impl Default for ZenMuxProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ZenMuxProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "ZenMux".to_string(),
                base_url: "https://zenmux.ai/api".to_string(),
                auth_mode: AuthMode::Bearer,
                channels: vec!["default".to_string()],
                protocols: vec![
                    ProtocolEndpoint::new(
                        ProtocolSuite::OpenAiCompatible,
                        "chat-completions",
                        "v1",
                    ),
                    ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
                    ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "v1"),
                ],
                defaults: serde_json::json!({}),
            },
        }
    }
}

impl Provider for ZenMuxProvider {
    fn id(&self) -> &str {
        "zenmux"
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn supported_protocols(&self) -> &[ProtocolEndpoint] {
        &self.metadata.protocols
    }

    fn auth(&self) -> Arc<dyn AuthApplier> {
        Arc::new(super::openai::BearerAuthApplier)
    }

    fn egress_protocol_for_model(&self, model_id: &str) -> ProtocolEndpoint {
        suite_for_model(model_id).default_endpoint()
    }

    fn egress_api_base(&self, raw_base: &str, endpoint: &ProtocolEndpoint) -> String {
        api_base_for_suite(raw_base, endpoint.suite)
    }
}

/// Pick the egress protocol suite for a ZenMux target from its egress
/// model id. The model id may be of the form `maker/model:provider`,
/// where the `maker/` prefix and `:provider` suffix are both optional.
fn suite_for_model(model_id: &str) -> ProtocolSuite {
    let body = model_id.split(':').next().unwrap_or(model_id);
    let body = body.rsplit('/').next().unwrap_or(body);
    let body = body.to_ascii_lowercase();

    if body.contains("gpt") {
        ProtocolSuite::OpenAiResponses
    } else if body.contains("claude") || body.contains("minimax") {
        ProtocolSuite::AnthropicMessages
    } else {
        ProtocolSuite::OpenAiCompatible
    }
}

/// Derive the egress api_base for a ZenMux target. The provider base is
/// expected to be the platform root, without a version segment. Known
/// version suffixes are stripped idempotently before appending the
/// suite-appropriate path.
fn api_base_for_suite(raw_base: &str, suite: ProtocolSuite) -> String {
    let mut base = raw_base.trim_end_matches('/');
    if let Some(stripped) = base.strip_suffix("/anthropic/v1") {
        base = stripped.trim_end_matches('/');
    } else if let Some(stripped) = base.strip_suffix("/v1") {
        base = stripped.trim_end_matches('/');
    }

    match suite {
        ProtocolSuite::AnthropicMessages => format!("{}/anthropic/v1", base),
        _ => format!("{}/v1", base),
    }
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(ZenMuxProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_zenmux_provider_metadata() {
        let provider = ZenMuxProvider::new();
        assert_eq!(provider.id(), "zenmux");
        assert_eq!(provider.metadata().display_name, "ZenMux");
        assert_eq!(provider.metadata().base_url, "https://zenmux.ai/api");
        assert!(matches!(provider.metadata().auth_mode, AuthMode::Bearer));
    }

    #[test]
    fn test_zenmux_supported_protocols() {
        let provider = ZenMuxProvider::new();
        let protocols = provider.supported_protocols();
        assert!(!protocols.is_empty());
    }

    #[test]
    fn test_zenmux_egress_protocol_for_model() {
        let provider = ZenMuxProvider::new();

        assert_eq!(
            provider.egress_protocol_for_model("gpt-5.5").suite,
            ProtocolSuite::OpenAiResponses
        );
        assert_eq!(
            provider
                .egress_protocol_for_model("anthropic/claude-sonnet-4:zenmux")
                .suite,
            ProtocolSuite::AnthropicMessages
        );
        assert_eq!(
            provider.egress_protocol_for_model("minimax-text-01").suite,
            ProtocolSuite::AnthropicMessages
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
    fn test_zenmux_egress_api_base_is_idempotent() {
        let provider = ZenMuxProvider::new();
        let responses = ProtocolSuite::OpenAiResponses.default_endpoint();
        let messages = ProtocolSuite::AnthropicMessages.default_endpoint();

        assert_eq!(
            provider.egress_api_base("https://example.test/api", &responses),
            "https://example.test/api/v1"
        );
        assert_eq!(
            provider.egress_api_base("https://example.test/api/v1/", &responses),
            "https://example.test/api/v1"
        );
        assert_eq!(
            provider.egress_api_base("https://example.test/api", &messages),
            "https://example.test/api/anthropic/v1"
        );
        assert_eq!(
            provider.egress_api_base("https://example.test/api/anthropic/v1", &messages),
            "https://example.test/api/anthropic/v1"
        );
    }
}
