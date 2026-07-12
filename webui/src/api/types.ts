// Types mirroring the tiygate-admin API view structs. Field names
// match the JSON wire format exactly (see crates/admin/src/handlers.rs).

export interface Provider {
  id: string;
  name: string;
  vendor: string;
  api_base: string;
  models_endpoint: string;
  auth_mode: string;
  encrypted_api_key: string;
  encrypted_oauth_meta: string;
  oauth_status?: {
    state: "not_connected" | "connected" | "healthy" | "invalid" | "error";
    reason?: string | null;
    checked_at?: string | null;
  } | null;
  metadata: Record<string, unknown>;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface ProviderInput {
  id?: string;
  name: string;
  vendor: string;
  api_base: string;
  models_endpoint?: string;
  api_key?: string;
  auth_mode?: string;
  oauth_meta?: string;
  metadata?: Record<string, unknown>;
  enabled?: boolean;
}

export interface ProviderDeleteImpactRoute {
  id: string;
  virtual_model: string;
  target_count: number;
  remaining_target_count: number;
  will_delete_route: boolean;
}

export interface ProviderDeleteImpact {
  provider_id: string;
  route_count: number;
  target_count: number;
  delete_route_count: number;
  routes: ProviderDeleteImpactRoute[];
}

export interface ProviderModelEntry {
  id: string;
}

export interface ProviderModelsResponse {
  models: ProviderModelEntry[];
}

export interface ProviderUsageWindow {
  used_percent?: number | null;
  reset_at?: number | null;
}

export type ProviderUsageState =
  | "available"
  | "not_connected"
  | "unsupported"
  | "unavailable";

export interface ProviderUsage {
  provider_id: string;
  state: ProviderUsageState;
  reason?: string | null;
  checked_at?: string | null;
  account_email?: string | null;
  plan_type?: string | null;
  five_hour?: ProviderUsageWindow | null;
  seven_day?: ProviderUsageWindow | null;
}

export interface ProviderCatalogEntry {
  id: string;
  display_name: string;
  default_base_url: string;
  auth_mode: string;
}

export interface ModelCatalogStatus {
  source: string;
  checksum: string;
  generated_at_unix: number;
  provider_count: number;
  model_count: number;
}

export type PricingSourceKind =
  | "official"
  | "openrouter_fallback"
  | "aggregator_fallback";

export interface ModelPricing {
  currency: string;
  input_token_usd_per_million?: number | null;
  output_token_usd_per_million?: number | null;
  cached_input_token_usd_per_million?: number | null;
  cached_write_token_usd_per_million?: number | null;
  source_provider: string;
  source_kind: PricingSourceKind;
}

export interface ModelMetadata {
  id: string;
  lab_id: string;
  display_name: string;
  family?: string | null;
  context_window?: number | null;
  max_input_tokens?: number | null;
  max_output_tokens?: number | null;
  capabilities?: Record<string, unknown>;
  modalities?: unknown;
  pricing?: ModelPricing | null;
  metadata?: Record<string, unknown>;
}

export interface ModelCatalogResolveRequest {
  virtual_model: string;
  target_model_id?: string;
}

/** Per-route routing strategy. Mirrors `tiygate_core::routing::RoutingStrategyName`
 *  (snake_case). `undefined`/absent means the route inherits the gateway-wide
 *  default strategy. */
export type RoutingStrategyName =
  "weighted" | "priority" | "cooldown" | "latency";

export interface RouteTarget {
  provider_id: string;
  model_id: string;
  // Backend persists only `weight`. The `priority` strategy reuses this same
  // value (sorted descending), so the UI just relabels the column per strategy.
  //
  // The `enabled` flag is mirrored from the server; missing values are
  // treated as enabled (the server defaults to `true`).
  weight?: number | null;
  enabled?: boolean;
}

export interface Route {
  id: string;
  virtual_model: string;
  targets: RouteTarget[];
  routing_strategy?: RoutingStrategyName | null;
  model_metadata?: ModelMetadata | null;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface RouteInput {
  id?: string;
  virtual_model: string;
  targets: RouteTarget[];
  routing_strategy?: RoutingStrategyName | null;
  model_metadata?: ModelMetadata | null;
  enabled?: boolean;
}

export interface RouteListResponse {
  total: number;
  limit: number;
  offset: number;
  entries: Route[];
}

export interface QuotaSpec {
  requests_per_minute?: number | null;
  requests_per_day?: number | null;
  tokens_per_minute?: number | null;
  tokens_per_day?: number | null;
}

export interface ApiKey {
  id: string;
  name: string;
  key_hash: string;
  quota: QuotaSpec;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface ApiKeyDetail extends ApiKey {
  usage: Partial<Record<keyof QuotaSpec, number>>;
}

export interface CreateApiKeyResponse {
  id: string;
  name: string;
  secret: string;
  quota: QuotaSpec;
  status: string;
  created_at: string;
}

export interface StatBucket {
  bucket: string;
  count: number;
  error_count: number;
  prompt_tokens: number;
  completion_tokens: number;
  reasoning_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_tokens: number;
  cost: number;
  avg_latency_ms?: number;
  avg_throughput_tps?: number;
}

export interface StatsResponse {
  since: string;
  until: string;
  buckets: StatBucket[];
}

export interface AuditChange {
  field: string;
  before: unknown;
  after: unknown;
}

export interface AuditDetails {
  snapshot?: Record<string, unknown>;
  changes?: AuditChange[];
}

export interface AuditEntry {
  id: number;
  actor: string;
  action: string;
  target_type: string;
  target_id: string;
  details: unknown;
  ts: string;
}

export interface AuditListResponse {
  total: number;
  limit: number;
  offset: number;
  entries: AuditEntry[];
}

export interface RequestLogEntry {
  request_id: string;
  ts: string;
  virtual_model: string;
  resolved_provider?: string | null;
  resolved_model?: string | null;
  account_label?: string | null;
  trace_id?: string | null;
  ingress_protocol?: string;
  egress_protocol?: string | null;
  lossy?: boolean;
  cache_hit?: string | null;
  status: string;
  error_class?: string | null;
  http_status?: number | null;
  error_source?: string | null;
  truncation_reason?: string | null;
  finish_reason?: string | null;
  total_latency_ms: number;
  upstream_latency_ms?: number;
  queue_latency_ms?: number;
  ttfb_ms?: number | null;
  stream_duration_ms?: number | null;
  prompt_tokens?: number | null;
  completion_tokens?: number | null;
  reasoning_tokens?: number | null;
  cache_read_tokens?: number | null;
  cache_write_tokens?: number | null;
  total_tokens?: number | null;
  cost?: number | null;
  input_cost?: number | null;
  output_cost?: number | null;
  cache_read_cost?: number | null;
  cache_write_cost?: number | null;
  api_key_id?: string | null;
  client_ip?: string | null;
  user_agent?: string | null;
  [key: string]: unknown;
}

export interface RequestListResponse {
  total: number;
  limit: number;
  offset: number;
  entries: RequestLogEntry[];
}

export interface RequestFilterOptions {
  models: string[];
  providers: string[];
  statuses: string[];
  error_classes: string[];
}

export interface RequestReplay {
  request_id: string;
  raw_envelope_json?: string | null;
  redacted_headers_json?: string | null;
  // Full exchange payload (joined from request_payloads).
  egress_method?: string | null;
  egress_path?: string | null;
  egress_headers_json?: string | null;
  egress_body?: string | null;
  upstream_status?: number | null;
  upstream_resp_headers_json?: string | null;
  upstream_resp_body?: string | null;
  client_resp_headers_json?: string | null;
  client_resp_body?: string | null;
  is_stream?: boolean;
  sse_parsed_json?: string | null;
  client_sse_parsed_json?: string | null;
  truncation_reason?: string | null;
  finish_reason?: string | null;
  payload_archive_status?: string | null;
  payload_archive_attempts?: number;
  payload_archive_last_error?: string | null;
  payload_archive_locked_at?: string | null;
  payload_archived_at?: string | null;
  payload_archive_manifest_json?: string | null;
}

export interface CircuitBreaker {
  target: string;
  provider_id?: string;
  provider_name?: string;
  model_id?: string;
  healthy: boolean;
  status: string;
  status_kind: "healthy" | "circuit_broken" | "cooling";
  remaining_seconds: number | null;
  cooling_reason: string | null;
  consecutive_failures: number;
  failure_threshold: number;
}

export interface CircuitBreakersResponse {
  targets: CircuitBreaker[];
  note?: string;
}

export interface OAuthStartResponse {
  url: string;
  state: string;
}

export interface OAuthTokenResponse {
  provider_id: string;
  expires_in_s?: number | null;
}

// ---- Token Activity (pre-aggregated dashboard panel) ----

export interface TokenDayActivity {
  day: string;
  total_tokens: number;
  total_cost: number;
  request_count: number;
}

export interface TokenActivityResponse {
  days: TokenDayActivity[];
}

export interface TokenSummaryData {
  lifetime_tokens: number;
  peak_day_tokens: number;
  lifetime_cost: number;
  peak_day_cost: number;
  longest_task_ms: number;
  current_streak: number;
  longest_streak: number;
  updated_at: string;
}

// ---- server info ----
export interface ServerInfo {
  name: string;
  version: string;
}

// ---- config export / import ----

/** Provider row inside an export bundle. Mirrors the Rust
 *  `Provider` model (snake_case), so it carries the encrypted
 *  secret columns rather than the `ProviderView` the list endpoint
 *  returns. */
export interface ExportProvider {
  id: string;
  name: string;
  vendor: string;
  api_base: string;
  models_endpoint: string;
  encrypted_api_key: string;
  auth_mode: string;
  encrypted_oauth_meta: string;
  metadata_json: Record<string, unknown>;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface ExportRouteTarget {
  provider_id: string;
  model_id: string;
  weight?: number;
  enabled?: boolean;
  account_label?: string | null;
  api_key_override?: string | null;
  api_base_override?: string | null;
}

export interface ExportRoute {
  id: string;
  virtual_model: string;
  targets: ExportRouteTarget[];
  routing_strategy?: RoutingStrategyName | null;
  model_metadata?: ModelMetadata | null;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface ExportApiKey {
  id: string;
  name: string;
  key_hash: string;
  quota_json: Record<string, unknown>;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface ExportSetting {
  key: string;
  value: string;
  encrypted: boolean;
}

/** One day of pre-aggregated token statistics from the export bundle.
 *  Mirrors the Rust `ExportTokenDailyStat` model. */
export interface ExportTokenDailyStat {
  day: string;
  request_count: number;
  total_tokens: number;
  prompt_tokens: number;
  completion_tokens: number;
  reasoning_tokens: number;
  total_cost: number;
  peak_single_request: number;
  longest_task_ms: number;
}

export interface ConfigExport {
  schema_version: number;
  exported_at: string;
  encrypted: boolean;
  providers: ExportProvider[];
  routes: ExportRoute[];
  api_keys: ExportApiKey[];
  settings?: ExportSetting[];
  token_daily_stats?: ExportTokenDailyStat[];
}

export interface ImportSelection {
  providers: string[];
  routes: string[];
  api_keys: string[];
  settings: string[];
  token_stats: string[];
}

export interface ImportReport {
  providers_imported: number;
  providers_skipped: number;
  routes_imported: number;
  routes_skipped: number;
  api_keys_imported: number;
  api_keys_skipped: number;
  settings_imported: number;
  settings_skipped: number;
  token_stats_imported: number;
  token_stats_skipped: number;
}

// ---- Settings ----

/** A flat map of setting key → string value. Sensitive (encrypted)
 * keys are redacted by the server (e.g. `[encrypted: abc…]`). */
export type Settings = Record<string, string>;

/** Runtime database metadata returned with authenticated settings. */
export interface SettingsDatabaseInfo {
  kind: "sqlite" | "postgres" | string;
}

/** Response body for `GET /admin/v1/settings` and `PUT /admin/v1/settings`. */
export interface SettingsResponse {
  settings: Settings;
  database?: SettingsDatabaseInfo;
}
