//! Moonshot AI provider implementation.

use std::sync::Arc;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct MoonshotProvider {
    metadata: ProviderMetadata,
}

impl Default for MoonshotProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MoonshotProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "Moonshot AI".to_string(),
                base_url: "https://api.moonshot.cn/v1".to_string(),
                auth_mode: AuthMode::Bearer,
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiCompatible,
                    "chat-completions",
                    "v1",
                )],
                defaults: serde_json::json!({}),
            },
        }
    }
}

impl Provider for MoonshotProvider {
    fn id(&self) -> &str {
        "moonshot"
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

    fn egress_protocol_for_model(&self, _model_id: &str) -> ProtocolEndpoint {
        ProtocolSuite::OpenAiCompatible.default_endpoint()
    }
}

inventory::submit! { tiygate_core::provider::ProviderRegistration { make: || Box::new(MoonshotProvider::new()) } }
