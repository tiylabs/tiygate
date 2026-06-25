//! Provider layer — traits for upstream model providers.
//!
//! Providers are registered via `inventory` for decentralized discovery.
//! Each provider implements the `Provider` trait with declarative metadata,
//! an `AuthApplier` for authentication, and optionally a custom `Executor`
//! for SDK-based providers (escape hatch).

use std::sync::Arc;

use http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::ir::{IrRequest, IrResponse};
use crate::pipeline::PipelineContext;
use crate::protocol::{ProtocolEndpoint, ProtocolSuite};

pub mod oauth;

/// Declarative metadata for a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMetadata {
    /// Human-readable provider name.
    pub display_name: String,
    /// Default base URL for the API.
    pub base_url: String,
    /// Authentication mode.
    pub auth_mode: AuthMode,
    /// Available account labels (channels).
    #[serde(default)]
    pub channels: Vec<String>,
    /// Supported protocol endpoints.
    pub protocols: Vec<ProtocolEndpoint>,
    /// Provider-specific configuration defaults.
    #[serde(default)]
    pub defaults: serde_json::Value,
}

/// How a provider authenticates requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Bearer token in Authorization header.
    Bearer,
    /// API key in a custom header.
    ApiKey { header_name: String },
    /// OAuth 2.0 client credentials.
    OAuth2,
    /// AWS Signature V4 (for Bedrock).
    AwsSigV4,
    /// Custom authentication (handled by AuthApplier).
    Custom,
}

/// The core provider trait.
///
/// Each upstream AI service implements this trait.
pub trait Provider: Send + Sync {
    /// Unique provider identifier (e.g., "openai", "anthropic").
    fn id(&self) -> &str;
    /// Declarative metadata.
    fn metadata(&self) -> &ProviderMetadata;
    /// Supported protocol endpoints.
    fn supported_protocols(&self) -> &[ProtocolEndpoint];
    /// Authentication handler.
    fn auth(&self) -> Arc<dyn AuthApplier>;
    /// Optional custom executor (None = use standard HTTP executor).
    fn executor(&self) -> Option<Arc<dyn Executor>> {
        None
    }

    /// Select the egress protocol endpoint for a concrete target model.
    ///
    /// Most providers use their first declared protocol endpoint for all
    /// models. Multi-protocol providers can override this to choose an
    /// endpoint from the egress model id.
    fn egress_protocol_for_model(&self, _model_id: &str) -> ProtocolEndpoint {
        self.supported_protocols()
            .first()
            .cloned()
            .unwrap_or_else(|| ProtocolSuite::OpenAiCompatible.default_endpoint())
    }

    /// Normalize or derive the upstream API base for a selected endpoint.
    ///
    /// The default keeps the configured base URL unchanged. Providers with
    /// suite-specific base paths can override this method.
    fn egress_api_base(&self, raw_base: &str, _endpoint: &ProtocolEndpoint) -> String {
        raw_base.to_string()
    }
}

/// Decentralized provider registration via `inventory`.
pub struct ProviderRegistration {
    pub make: fn() -> Box<dyn Provider>,
}

inventory::collect!(ProviderRegistration);

/// Iterate all registered providers (the providers' `submit!` calls
/// register a factory; this function builds an instance of each and
/// yields it). Returns a `Vec` because `inventory` exposes only a
/// static slice, and we want callers to be able to look up by
/// `id()` (e.g. "anthropic" → `AnthropicProvider::auth()`).
pub fn all_providers() -> Vec<Box<dyn Provider>> {
    inventory::iter::<ProviderRegistration>()
        .map(|reg| (reg.make)())
        .collect()
}

/// Look up a registered provider by its `id()`.
pub fn find_provider(id: &str) -> Option<Box<dyn Provider>> {
    inventory::iter::<ProviderRegistration>()
        .map(|reg| (reg.make)())
        .find(|p| p.id() == id)
}

/// Authentication applier — applies credentials to outgoing requests.
///
/// The auth applier is decoupled from the protocol layer.
/// Token refresh uses single-flight semantics (per-label `tokio::sync::Mutex`)
/// to prevent concurrent refreshes from invalidating each other.
#[async_trait::async_trait]
pub trait AuthApplier: Send + Sync {
    /// Apply authentication headers to a request.
    async fn apply(
        &self,
        headers: &mut HeaderMap,
        target: &crate::routing::RoutingTarget,
    ) -> Result<(), crate::Error>;

    /// Optionally modify the request body before sending (for OAuth subscription providers).
    async fn prepare_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &crate::routing::RoutingTarget,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Executor trait — the abstraction for calling an upstream provider.
///
/// The standard HTTP executor handles HTTP+JSON+SSE providers.
/// SDK-based providers (AWS Bedrock, etc.) implement their own `Executor`
/// and completely bypass the standard HTTP path.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    /// Execute a non-streaming request against a target.
    async fn execute(
        &self,
        target: &crate::routing::RoutingTarget,
        ir: &IrRequest,
        ctx: &PipelineContext,
    ) -> Result<IrResponse, crate::Error>;

    /// Execute a streaming request against a target.
    async fn execute_stream(
        &self,
        target: &crate::routing::RoutingTarget,
        ir: &IrRequest,
        ctx: &PipelineContext,
    ) -> Result<crate::protocol::StreamPartStream, crate::Error>;
}
