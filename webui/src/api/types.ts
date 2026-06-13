// Types mirroring the tiygate-admin API view structs. Field names
// match the JSON wire format exactly (see crates/admin/src/handlers.rs).

export interface Provider {
  id: string;
  name: string;
  vendor: string;
  api_base: string;
  auth_mode: string;
  encrypted_api_key: string;
  encrypted_oauth_meta: string;
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
  api_key?: string;
  auth_mode?: string;
  oauth_meta?: string;
  metadata?: Record<string, unknown>;
  enabled?: boolean;
}

/** One entry of the server-side provider catalog
 *  (`GET /admin/v1/provider-catalog`). Reflects the providers actually
 *  registered/compiled into the gateway binary, so the set changes with
 *  feature flags rather than a hardcoded frontend list. */
export interface ProviderCatalogEntry {
  id: string;
  display_name: string;
  default_base_url: string;
  auth_mode: string;
}

export interface RouteTarget {
  provider_id: string;
  model_id: string;
  weight?: number | null;
  priority?: number | null;
}

export interface Route {
  id: string;
  virtual_model: string;
  targets: RouteTarget[];
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface RouteInput {
  id?: string;
  virtual_model: string;
  targets: RouteTarget[];
  enabled?: boolean;
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
  total_latency_ms: number;
  upstream_latency_ms?: number;
  queue_latency_ms?: number;
  ttfb_ms?: number | null;
  prompt_tokens?: number | null;
  completion_tokens?: number | null;
  reasoning_tokens?: number | null;
  cache_read_tokens?: number | null;
  cache_write_tokens?: number | null;
  total_tokens?: number | null;
  cost?: number | null;
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

export interface RequestReplay {
  request_id: string;
  raw_envelope_json?: string | null;
  redacted_headers_json?: string | null;
  // Full exchange payload (joined from request_payloads).
  egress_method?: string | null;
  egress_path?: string | null;
  egress_headers_json?: string | null;
  egress_body?: string | null;
  egress_body_truncated?: boolean;
  upstream_status?: number | null;
  upstream_resp_headers_json?: string | null;
  upstream_resp_body?: string | null;
  upstream_resp_body_truncated?: boolean;
  client_resp_headers_json?: string | null;
  client_resp_body?: string | null;
  client_resp_body_truncated?: boolean;
  is_stream?: boolean;
  sse_parsed_json?: string | null;
  client_sse_parsed_json?: string | null;
}

export interface CircuitBreaker {
  target: string;
  provider_id?: string;
  provider_name?: string;
  model_id?: string;
  healthy: boolean;
  status: string;
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
  access_token?: string | null;
  expires_in_s?: number | null;
}
