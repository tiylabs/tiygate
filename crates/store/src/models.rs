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
    /// Optional model-discovery endpoint (e.g. `GET /v1/models`).
    /// Backward-compatible: empty string means "not configured".
    #[serde(default)]
    pub models_endpoint: String,
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
    /// Decrypted cleartext of `encrypted_oauth_meta`. Populated by
    /// `DbConfigStore::refresh()` for OAuth-mode providers so
    /// `snapshot_to_routing_table` can extract the refresh token
    /// without touching the DB or crypto on the hot path. `None`
    /// when no master key is configured or the provider is not
    /// OAuth-mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_meta_cleartext: Option<String>,
}

/// A single routing target inside a route's chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteTarget {
    pub provider_id: String,
    pub model_id: String,
    /// Per-target weight. Defaults to `1.0` when omitted (e.g. for the
    /// `cooldown`/`latency` strategies that do not consume an explicit value).
    #[serde(default = "default_target_weight")]
    pub weight: f64,
    /// Per-target enabled flag. Defaults to `true` so that pre-existing
    /// `targets_json` payloads written before this field existed continue
    /// to deserialize to a working, enabled target. When `false`, the
    /// `config_store` skips this target while building the runtime
    /// `RoutingTable`, effectively removing it from any strategy.
    #[serde(default = "default_target_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_override: Option<String>,
}

/// Default weight for a [`RouteTarget`] when the field is omitted from input.
fn default_target_weight() -> f64 {
    1.0
}

/// Default enabled flag for a [`RouteTarget`] when the field is omitted.
/// Old persisted rows and incoming requests without the field keep
/// treating the target as live.
fn default_target_enabled() -> bool {
    true
}

/// A virtual model name → ordered target chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: String,
    pub virtual_model: String,
    pub targets: Vec<RouteTarget>,
    /// Optional per-route routing strategy override. `None` means the
    /// route inherits the gateway-wide default (`ServerConfig.routing_strategy`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
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

/// A single setting row from the `settings` table, carried in a
/// config export/import bundle. `encrypted` indicates whether the
/// `value` is an AES-GCM ciphertext blob (when the source instance
/// had a master key configured for that key) or plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSetting {
    pub key: String,
    pub value: String,
    pub encrypted: bool,
}

/// A single day of pre-aggregated token activity from the
/// `token_daily_stats` table, carried in a config export/import
/// bundle. The `updated_at` column is excluded — it is refreshed by
/// the importing instance on merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportTokenDailyStat {
    pub day: String,
    pub request_count: i64,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub reasoning_tokens: i64,
    pub peak_single_request: i64,
    pub longest_task_ms: i64,
}

/// Operator-selected subset of an import bundle. Each vec carries
/// the ids (or setting keys) the user explicitly chose to import.
/// Items present in the bundle but absent from the selection are
/// skipped. An empty selection imports nothing — the frontend is
/// responsible for pre-selecting new ids and leaving existing ids
/// unchecked by default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportSelection {
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub routes: Vec<String>,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub settings: Vec<String>,
    /// Selected `day` strings (YYYY-MM-DD) from the export bundle's
    /// `token_daily_stats` section. Token stats use additive merge
    /// (sum / MAX) rather than overwrite, so these are safe to
    /// import repeatedly.
    #[serde(default)]
    pub token_stats: Vec<String>,
}

/// A serializable bundle of all configurable entities, used by the
/// config export / import endpoints. Provider secrets are carried as
/// their on-disk encrypted blobs; the `encrypted` flag tells the
/// importer whether a master key is needed to decode them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigExport {
    /// Schema version of the export envelope. Bumped when the
    /// structure changes in a backwards-incompatible way.
    pub schema_version: u32,
    /// RFC-3339 timestamp of when the export was produced.
    pub exported_at: String,
    /// Whether the source instance had a master key configured. When
    /// `true`, provider `encrypted_api_key` / `encrypted_oauth_meta`
    /// are real AES-GCM blobs that need the source master key to
    /// decrypt. When `false`, those columns hold cleartext.
    pub encrypted: bool,
    pub providers: Vec<Provider>,
    pub routes: Vec<Route>,
    pub api_keys: Vec<ApiKey>,
    /// Settings table rows. Absent on exports produced before this
    /// field was added; `#[serde(default)]` makes old bundles
    /// deserialize cleanly to an empty vec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<ExportSetting>,
    /// Pre-aggregated daily token statistics. Absent on exports
    /// produced before this field was added; `#[serde(default)]`
    /// makes old bundles deserialize cleanly to an empty vec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_daily_stats: Vec<ExportTokenDailyStat>,
}

/// Summary of an import operation, returned to the caller so the UI
/// can show how many rows were actually inserted vs. skipped due to
/// an existing id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub providers_imported: usize,
    pub providers_skipped: usize,
    pub routes_imported: usize,
    pub routes_skipped: usize,
    pub api_keys_imported: usize,
    pub api_keys_skipped: usize,
    pub settings_imported: usize,
    pub settings_skipped: usize,
    pub token_stats_imported: usize,
    pub token_stats_skipped: usize,
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
