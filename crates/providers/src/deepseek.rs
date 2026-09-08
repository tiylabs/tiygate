//! DeepSeek provider implementation.

use std::sync::Arc;
use tiygate_auth::bearer::BearerAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
};

pub struct DeepSeekProvider {
    metadata: ProviderMetadata,
}

impl Default for DeepSeekProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DeepSeekProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "DeepSeek".to_string(),
                base_url: "https://api.deepseek.com".to_string(),
                auth_mode: AuthMode::Bearer,
                channels: vec!["default".to_string()],
                protocols: vec![
                    deepseek_responses_endpoint(),
                    ProtocolEndpoint::new(
                        ProtocolSuite::OpenAiCompatible,
                        "chat-completions",
                        "v1",
                    ),
                ],
                defaults: serde_json::json!({}),
            },
        }
    }
}

impl Provider for DeepSeekProvider {
    fn id(&self) -> &str {
        "deepseek"
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

    fn egress_protocol_for_model(&self, model_id: &str) -> ProtocolEndpoint {
        if uses_chat_completions(model_id) {
            ProtocolSuite::OpenAiCompatible.default_endpoint()
        } else {
            deepseek_responses_endpoint()
        }
    }

    fn egress_api_base(&self, raw_base: &str, endpoint: &ProtocolEndpoint) -> String {
        deepseek_api_base(raw_base, endpoint.suite)
    }
}

fn deepseek_responses_endpoint() -> ProtocolEndpoint {
    ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "deepseek-responses", "v1")
}

fn normalized_model_id(model_id: &str) -> String {
    let without_provider = model_id.split(':').next().unwrap_or(model_id);
    without_provider
        .rsplit('/')
        .next()
        .unwrap_or(without_provider)
        .to_ascii_lowercase()
}

fn uses_chat_completions(model_id: &str) -> bool {
    matches!(
        normalized_model_id(model_id).as_str(),
        "deepseek-chat" | "deepseek-reasoner"
    )
}

fn deepseek_api_base(raw_base: &str, suite: ProtocolSuite) -> String {
    let base = raw_base.trim_end_matches('/');
    let base = base
        .strip_suffix("/v1")
        .unwrap_or(base)
        .trim_end_matches('/');

    match suite {
        ProtocolSuite::OpenAiResponses => base.to_string(),
        ProtocolSuite::OpenAiCompatible => format!("{base}/v1"),
        _ => raw_base.trim_end_matches('/').to_string(),
    }
}

inventory::submit! { tiygate_core::provider::ProviderRegistration { make: || Box::new(DeepSeekProvider::new()) } }

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn declares_responses_and_chat_completions() {
        let provider = DeepSeekProvider::new();
        let suites: Vec<_> = provider
            .supported_protocols()
            .iter()
            .map(|protocol| protocol.suite)
            .collect();

        assert_eq!(provider.metadata().base_url, "https://api.deepseek.com");
        assert_eq!(provider.supported_protocols()[0].name, "deepseek-responses");
        assert_eq!(
            suites,
            vec![
                ProtocolSuite::OpenAiResponses,
                ProtocolSuite::OpenAiCompatible
            ]
        );
    }

    #[test]
    fn only_legacy_chat_models_use_chat_completions() {
        let provider = DeepSeekProvider::new();

        for model in [
            "deepseek-v4-flash",
            "deepseek/deepseek-v4-pro:official",
            "deepseek-v4-flash-vision-exp",
            "deepseek-v5-preview",
            "unknown-model",
        ] {
            assert_eq!(
                provider.egress_protocol_for_model(model).suite,
                ProtocolSuite::OpenAiResponses,
                "{model} should use Responses"
            );
            assert_eq!(
                provider.egress_protocol_for_model(model).name,
                "deepseek-responses"
            );
        }

        for model in [
            "deepseek-chat",
            "deepseek-reasoner",
            "deepseek/deepseek-chat:official",
            "deepseek/deepseek-reasoner:official",
        ] {
            assert_eq!(
                provider.egress_protocol_for_model(model).suite,
                ProtocolSuite::OpenAiCompatible,
                "{model} should keep Chat Completions"
            );
        }
    }

    #[test]
    fn api_base_is_normalized_per_protocol() {
        let provider = DeepSeekProvider::new();
        let responses = ProtocolSuite::OpenAiResponses.default_endpoint();
        let chat = ProtocolSuite::OpenAiCompatible.default_endpoint();

        assert_eq!(
            provider.egress_api_base("https://api.deepseek.com/v1/", &responses),
            "https://api.deepseek.com"
        );
        assert_eq!(
            provider.egress_api_base("https://api.deepseek.com", &chat),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            provider.egress_api_base("https://proxy.example/v1", &chat),
            "https://proxy.example/v1"
        );
    }
}
