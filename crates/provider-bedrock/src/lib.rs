//! TiyGate Bedrock Provider — SDK-based escape hatch for AWS Bedrock.
//!
//! This crate demonstrates the Executor escape hatch pattern,
//! implementing a custom executor that uses AWS SigV4 signing
//! instead of standard HTTP+JSON+SSE adapter.
//!
//! ## Inventory registration
//!
//! The provider registers itself via `inventory::submit!` so the
//! gateway can look it up by `id()` ("bedrock") at runtime. The
//! `AuthApplier` for Bedrock returns SigV4-signed headers via the
//! standard provider flow; the actual signing happens in
//! `BedrockExecutor::sign_request` (called from the executor path).

pub mod executor;

pub use executor::{AwsCredentials, BedrockExecutor};

use std::sync::Arc;

use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

/// Bedrock provider metadata.
pub struct BedrockProvider {
    metadata: ProviderMetadata,
    auth: Arc<dyn AuthApplier>,
}

impl Default for BedrockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BedrockProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "AWS Bedrock".to_string(),
                base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
                auth_mode: AuthMode::AwsSigV4,
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolEndpoint::new(
                    ProtocolSuite::AnthropicMessages,
                    "messages",
                    "2023-06-01",
                )],
                defaults: serde_json::json!({}),
            },
            // Bedrock uses an API key in the `access:secret:region`
            // format. The `ApiKeyAuthApplier` style works for the
            // header-name convention; the actual SigV4 signing lives
            // in the executor. We reuse a custom applier that simply
            // injects a placeholder Bearer header so the egress path
            // is consistent; the executor overrides at sign time.
            auth: Arc::new(BedrockAuthApplier),
        }
    }
}

impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        "bedrock"
    }
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }
    fn supported_protocols(&self) -> &[ProtocolEndpoint] {
        &self.metadata.protocols
    }
    fn auth(&self) -> Arc<dyn AuthApplier> {
        self.auth.clone()
    }
    fn executor(&self) -> Option<Arc<dyn tiygate_core::Executor>> {
        Some(Arc::new(BedrockExecutor::new()))
    }
}

/// Bedrock-specific AuthApplier. The actual signing happens in the
/// executor; this applier only injects a marker header so the egress
/// path can identify Bedrock requests. The executor strips / replaces
/// this header with the real SigV4 `Authorization`.
struct BedrockAuthApplier;

#[async_trait::async_trait]
impl AuthApplier for BedrockAuthApplier {
    async fn apply(
        &self,
        headers: &mut http::HeaderMap,
        _target: &tiygate_core::RoutingTarget,
    ) -> Result<(), tiygate_core::Error> {
        // No-op: the executor signs the request before sending.
        // We still need to populate at least one header so the
        // upstream URL is not sent unauthenticated. The executor
        // re-signs and replaces any auth headers.
        if !headers.contains_key(http::header::AUTHORIZATION) {
            headers.insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_static("AWS4-HMAC-SHA256 placeholder"),
            );
        }
        Ok(())
    }
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(BedrockProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tiygate_core::RoutingTarget;

    #[test]
    fn bedrock_provider_registered() {
        let providers = tiygate_core::provider::all_providers();
        assert!(
            providers.iter().any(|p| p.id() == "bedrock"),
            "BedrockProvider must be registered via inventory"
        );
    }

    #[test]
    fn bedrock_provider_has_executor() {
        let provider = BedrockProvider::new();
        let exec = provider.executor();
        assert!(exec.is_some(), "BedrockProvider must expose its executor");
    }

    #[tokio::test]
    async fn bedrock_auth_applier_populates_authorization() {
        let applier = BedrockAuthApplier;
        let target = RoutingTarget {
            provider_id: "bedrock".to_string(),
            model_id: "anthropic.claude-sonnet-4-20250514-v1:0".to_string(),
            api_base: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
            api_key: "AKID:secret:us-east-1".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        };
        let mut headers = http::HeaderMap::new();
        applier.apply(&mut headers, &target).await.unwrap();
        assert!(headers.contains_key(http::header::AUTHORIZATION));
    }
}
