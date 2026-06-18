//! Ollama provider implementation.

use std::sync::Arc;
use tiygate_auth::bearer::BearerAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct OllamaProvider {
    metadata: ProviderMetadata,
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "Ollama".to_string(),
                base_url: "http://localhost:11434/v1".to_string(),
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

impl Provider for OllamaProvider {
    fn id(&self) -> &str {
        "ollama"
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

    fn egress_protocol_for_model(&self, _model_id: &str) -> ProtocolEndpoint {
        ProtocolSuite::OpenAiCompatible.default_endpoint()
    }
}

inventory::submit! { tiygate_core::provider::ProviderRegistration { make: || Box::new(OllamaProvider::new()) } }
