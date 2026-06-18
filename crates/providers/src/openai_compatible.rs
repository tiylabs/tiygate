//! OpenAI-compatible provider — generic provider for any OpenAI-API-compatible service.
//!
//! This provider uses env vars to detect and configure custom OpenAI-compatible endpoints.
//! Supports services like Ollama, vLLM, local proxies, etc.

use std::sync::Arc;

use tiygate_auth::bearer::BearerAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

#[allow(dead_code)]
pub struct OpenAiCompatibleProvider {
    metadata: ProviderMetadata,
    base_url: String,
    api_key: String,
}

impl OpenAiCompatibleProvider {
    /// Create from environment variables.
    /// Reads `OPENAI_COMPATIBLE_BASE_URL` and `OPENAI_COMPATIBLE_API_KEY`.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OPENAI_COMPATIBLE_BASE_URL").ok()?;
        let api_key =
            std::env::var("OPENAI_COMPATIBLE_API_KEY").unwrap_or_else(|_| "not-needed".to_string());

        Some(Self {
            base_url: base_url.clone(),
            api_key,
            metadata: ProviderMetadata {
                display_name: "OpenAI Compatible".to_string(),
                base_url: base_url.clone(),
                auth_mode: AuthMode::Bearer,
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiCompatible,
                    "chat-completions",
                    "v1",
                )],
                defaults: serde_json::json!({}),
            },
        })
    }
}

impl Provider for OpenAiCompatibleProvider {
    fn id(&self) -> &str {
        "openai-compatible"
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

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(OpenAiCompatibleProvider::from_env().unwrap_or_else(|| {
            // Fallback with empty defaults when env not set
            OpenAiCompatibleProvider {
                base_url: "http://localhost:8080/v1".to_string(),
                api_key: "not-needed".to_string(),
                metadata: tiygate_core::ProviderMetadata {
                    display_name: "OpenAI Compatible".to_string(),
                    base_url: "http://localhost:8080/v1".to_string(),
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
        })),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_openai_compatible_from_env_with_var() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("OPENAI_COMPATIBLE_BASE_URL", "https://test.api/v1");
        let provider = OpenAiCompatibleProvider::from_env().unwrap();
        assert_eq!(provider.id(), "openai-compatible");
        assert_eq!(provider.metadata().base_url, "https://test.api/v1");
        std::env::remove_var("OPENAI_COMPATIBLE_BASE_URL");
    }

    #[test]
    fn test_openai_compatible_without_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("OPENAI_COMPATIBLE_BASE_URL");
        let provider = OpenAiCompatibleProvider::from_env();
        assert!(provider.is_none());
    }

    #[test]
    fn test_openai_compatible_protocols() {
        let _lock = ENV_MUTEX.lock().unwrap();
        std::env::set_var("OPENAI_COMPATIBLE_BASE_URL", "https://test.api/v1");
        let provider = OpenAiCompatibleProvider::from_env().unwrap();
        let protocols = provider.supported_protocols();
        assert!(!protocols.is_empty());
        assert_eq!(protocols[0].suite, ProtocolSuite::OpenAiCompatible);
        std::env::remove_var("OPENAI_COMPATIBLE_BASE_URL");
    }
}
