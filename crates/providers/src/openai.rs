//! OpenAI provider implementation.

use std::sync::Arc;

use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct OpenAiProvider {
    metadata: ProviderMetadata,
}

impl Default for OpenAiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "OpenAI".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                auth_mode: AuthMode::Bearer,
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolSuite::OpenAiResponses.default_endpoint()],
                defaults: serde_json::json!({}),
            },
        }
    }
}

impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
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

/// Bearer token authentication applier.
pub struct BearerAuthApplier;

#[async_trait::async_trait]
impl AuthApplier for BearerAuthApplier {
    async fn apply(
        &self,
        headers: &mut http::HeaderMap,
        target: &tiygate_core::RoutingTarget,
    ) -> Result<(), tiygate_core::Error> {
        let key = target.effective_api_key();
        let header_value = http::HeaderValue::from_str(&format!("Bearer {}", key))
            .map_err(|e| tiygate_core::Error::Auth(format!("Invalid header value: {}", e)))?;
        headers.insert(http::header::AUTHORIZATION, header_value);
        Ok(())
    }
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(OpenAiProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_provider_metadata() {
        let provider = OpenAiProvider::new();
        assert_eq!(provider.id(), "openai");
        assert_eq!(provider.metadata().display_name, "OpenAI");
        assert_eq!(provider.metadata().base_url, "https://api.openai.com/v1");
        assert!(matches!(provider.metadata().auth_mode, AuthMode::Bearer));
        assert_eq!(provider.metadata().channels.len(), 1);
        assert_eq!(provider.metadata().channels[0], "default");
    }

    #[test]
    fn test_openai_supported_protocols() {
        let provider = OpenAiProvider::new();
        let protocols = provider.supported_protocols();
        assert!(!protocols.is_empty());
        assert_eq!(protocols[0].suite, ProtocolSuite::OpenAiResponses);
    }

    #[test]
    fn test_openai_auth_applier() {
        let provider = OpenAiProvider::new();
        let auth = provider.auth();
        // AuthApplier should exist
        assert!(std::sync::Arc::strong_count(&auth) >= 1);
    }
}
