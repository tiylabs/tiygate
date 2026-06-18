//! Anthropic provider implementation.

use std::sync::Arc;

use tiygate_auth::api_key::HeaderApiKeyAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct AnthropicProvider {
    metadata: ProviderMetadata,
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "Anthropic".to_string(),
                base_url: "https://api.anthropic.com/v1".to_string(),
                auth_mode: AuthMode::ApiKey {
                    header_name: "x-api-key".to_string(),
                },
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolEndpoint::new(
                    ProtocolSuite::AnthropicMessages,
                    "messages",
                    "2023-06-01",
                )],
                defaults: serde_json::json!({
                    "anthropic_version": "2023-06-01"
                }),
            },
        }
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn supported_protocols(&self) -> &[ProtocolEndpoint] {
        &self.metadata.protocols
    }

    fn auth(&self) -> Arc<dyn AuthApplier> {
        Arc::new(HeaderApiKeyAuthApplier {
            header_name: "x-api-key".to_string(),
        })
    }
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(AnthropicProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_provider_metadata() {
        let provider = AnthropicProvider::new();
        assert_eq!(provider.id(), "anthropic");
        assert_eq!(provider.metadata().display_name, "Anthropic");
        assert_eq!(provider.metadata().base_url, "https://api.anthropic.com/v1");
        assert_eq!(provider.metadata().channels.len(), 1);
    }

    #[test]
    fn test_anthropic_supported_protocols() {
        let provider = AnthropicProvider::new();
        let protocols = provider.supported_protocols();
        assert!(!protocols.is_empty());
        assert_eq!(protocols[0].suite, ProtocolSuite::AnthropicMessages);
    }

    #[test]
    fn test_anthropic_auth_applier() {
        let provider = AnthropicProvider::new();
        let auth = provider.auth();
        assert!(std::sync::Arc::strong_count(&auth) >= 1);
    }
}
