//! Zhipu AI (GLM) provider implementation.

use std::sync::Arc;
use tiygate_auth::bearer::BearerAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct ZhipuProvider {
    metadata: ProviderMetadata,
}

impl Default for ZhipuProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ZhipuProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "Zhipu AI".to_string(),
                base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
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

impl Provider for ZhipuProvider {
    fn id(&self) -> &str {
        "zhipu"
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

inventory::submit! { tiygate_core::provider::ProviderRegistration { make: || Box::new(ZhipuProvider::new()) } }
