//! Domain models persisted in the config database.

use serde::{Deserialize, Serialize};

/// Authentication mode for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Static API key (bearer / x-api-key / etc.).
    ApiKey,
    /// OAuth 2.0 client-credentials or authorization-code.
    OAuth,
    /// AWS IAM (Bedrock and similar).
    Iam,
    /// No credentials (e.g. local Ollama, mocks).
    None,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::ApiKey => "api_key",
            AuthMode::OAuth => "oauth",
            AuthMode::Iam => "iam",
            AuthMode::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "api_key" => Some(Self::ApiKey),
            "oauth" => Some(Self::OAuth),
            "iam" => Some(Self::Iam),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// A registered upstream provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub api_base: String,
    /// Encrypted API key (or empty). Set at upsert time; the
    /// `DbConfigStore` populates `api_key_cleartext` on read.
    pub encrypted_api_key: String,
    pub auth_mode: AuthMode,
    /// Encrypted OAuth metadata JSON, or empty.
    pub encrypted_oauth_meta: String,
    pub metadata_json: serde_json::Value,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Decrypted cleartext of `encrypted_api_key`. Populated by
    /// `DbConfigStore::refresh()` so the data plane can hand the
    /// credential to the upstream call without re-running the
    /// crypto each time. `None` when no master key is configured
    /// (the gateway runs in "cleartext-fallback" mode in that case,
    /// and the cleartext lives in `encrypted_api_key` verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_cleartext: Option<String>,
}

/// A single routing target inside a route's chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteTarget {
    pub provider_id: String,
    pub model_id: String,
    pub weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_override: Option<String>,
}

/// A virtual model name → ordered target chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: String,
    pub virtual_model: String,
    pub targets: Vec<RouteTarget>,
    pub enabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// API key status (Phase 4: 创建—启用—删除 三态).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyStatus {
    Active,
    Disabled,
}

impl ApiKeyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// A caller-side API key. The cleartext secret is *never* stored —
/// only a SHA-256 hash. The cleartext is returned once to the admin
/// caller on creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub key_hash: String,
    pub quota_json: serde_json::Value,
    pub status: ApiKeyStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// An entry from the `config_epoch` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEpoch {
    pub epoch: i64,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Default for ConfigEpoch {
    fn default() -> Self {
        Self {
            epoch: 0,
            updated_at: chrono::Utc::now(),
        }
    }
}

/// The full in-memory snapshot used by the data plane. Built from
/// the union of providers + routes; refreshed on every epoch tick.
#[derive(Debug, Clone, Default)]
pub struct ConfigSnapshot {
    /// The current epoch. Increments on every admin write.
    pub epoch: i64,
    /// Provider registry, keyed by id. Cleared secrets are stored
    /// *encrypted* — decrypt just-in-time on the hot path or hold
    /// the decrypted form in the snapshot (the design doc §5 says
    /// "config 无状态"; we keep the decrypted form in memory because
    /// the upstream call site needs the cleartext).
    pub providers: std::collections::HashMap<String, Provider>,
    /// Routing table, keyed by virtual model name.
    pub routes: std::collections::HashMap<String, Route>,
}
