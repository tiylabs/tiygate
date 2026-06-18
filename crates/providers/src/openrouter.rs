//! OpenRouter provider implementation.

use std::sync::Arc;

use tiygate_auth::bearer::BearerAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct OpenRouterProvider {
    metadata: ProviderMetadata,
}

impl Default for OpenRouterProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "OpenRouter".to_string(),
                base_url: "https://openrouter.ai/api/v1".to_string(),
                auth_mode: AuthMode::Bearer,
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolSuite::OpenAiResponses.default_endpoint()],
                defaults: serde_json::json!({}),
            },
        }
    }
}

impl Provider for OpenRouterProvider {
    fn id(&self) -> &str {
        "openrouter"
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
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(OpenRouterProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_openrouter_provider_metadata() {
        let provider = OpenRouterProvider::new();
        assert_eq!(provider.id(), "openrouter");
        assert_eq!(provider.metadata().display_name, "OpenRouter");
        assert_eq!(provider.metadata().base_url, "https://openrouter.ai/api/v1");
        assert!(matches!(provider.metadata().auth_mode, AuthMode::Bearer));
        assert_eq!(provider.metadata().channels.len(), 1);
        assert_eq!(provider.metadata().channels[0], "default");
    }

    #[test]
    fn test_openrouter_supported_protocols() {
        let provider = OpenRouterProvider::new();
        let protocols = provider.supported_protocols();
        assert!(!protocols.is_empty());
        assert_eq!(protocols[0].suite, ProtocolSuite::OpenAiResponses);
    }

    #[test]
    fn test_openrouter_auth_applier() {
        let provider = OpenRouterProvider::new();
        let auth = provider.auth();
        assert!(std::sync::Arc::strong_count(&auth) >= 1);
    }
}
