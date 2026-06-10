//! Configuration storage (providers, routes, API keys) via sqlx.

use tiygate_core::RoutingTable;

/// In-memory configuration store (Phase 1: simple, Phase 4: DB-backed).
#[derive(Clone)]
pub struct ConfigStore {
    pub routing_table: RoutingTable,
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigStore {
    pub fn new() -> Self {
        Self {
            routing_table: RoutingTable::new(),
        }
    }

    /// Build a default routing table from environment variables.
    /// Detects OPENAI_API_KEY and ANTHROPIC_API_KEY to auto-configure routes.
    pub fn from_env() -> Self {
        let mut store = Self::new();
        let mut table = RoutingTable::new();

        // OpenAI auto-config
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            use tiygate_core::{ProtocolEndpoint, ProtocolSuite, RoutingTarget};

            let openai_targets = vec![RoutingTarget {
                provider_id: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
                api_base: "https://api.openai.com/v1".to_string(),
                api_key: key.clone(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiCompatible,
                    "chat-completions",
                    "v1",
                ),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            }];

            table.insert("gpt-4o".to_string(), openai_targets.clone());
            table.insert("gpt-4o-mini".to_string(), {
                let mut t = openai_targets.clone();
                t[0].model_id = "gpt-4o-mini".to_string();
                t
            });
            table.insert("gpt-3.5-turbo".to_string(), {
                let mut t = openai_targets;
                t[0].model_id = "gpt-3.5-turbo".to_string();
                t
            });
        }

        // Anthropic auto-config
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            let anthropic_targets = vec![tiygate_core::RoutingTarget {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-20250514".to_string(),
                api_base: "https://api.anthropic.com/v1".to_string(),
                api_key: key.clone(),
                api_protocol: tiygate_core::ProtocolEndpoint::new(
                    tiygate_core::ProtocolSuite::AnthropicMessages,
                    "messages",
                    "2023-06-01",
                ),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            }];

            table.insert("claude-sonnet-4-20250514".to_string(), anthropic_targets);
        }

        store.routing_table = table;
        store
    }
}
