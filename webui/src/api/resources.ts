import { apiRequest } from "./client";
import type {
  ApiKey,
  ApiKeyDetail,
  AuditListResponse,
  CircuitBreakersResponse,
  ConfigExport,
  CreateApiKeyResponse,
  ImportReport,
  ImportSelection,
  OAuthStartResponse,
  OAuthTokenResponse,
  ModelCatalogResolveRequest,
  ModelCatalogStatus,
  ModelMetadata,
  Provider,
  ProviderCatalogEntry,
  ProviderDeleteImpact,
  ProviderInput,
  ProviderModelsResponse,
  ProviderUsage,
  QuotaSpec,
  RequestFilterOptions,
  RequestListResponse,
  RequestReplay,
  Route,
  RouteInput,
  RouteListResponse,
  ServerInfo,
  Settings,
  SettingsResponse,
  StatsResponse,
  TokenActivityResponse,
  TokenSummaryData,
} from "./types";

// ---- providers ----
export const providersApi = {
  list: () => apiRequest<Provider[]>("/providers"),
  get: (id: string) => apiRequest<Provider>(`/providers/${id}`),
  deleteImpact: (id: string) =>
    apiRequest<ProviderDeleteImpact>(`/providers/${id}/delete-impact`),
  create: (body: ProviderInput) =>
    apiRequest<Provider>("/providers", { method: "POST", body }),
  update: (id: string, body: ProviderInput) =>
    apiRequest<Provider>(`/providers/${id}`, { method: "PUT", body }),
  remove: (id: string) =>
    apiRequest<void>(`/providers/${id}`, {
      method: "DELETE",
      allowEmpty: true,
    }),
  models: (id: string) =>
    apiRequest<ProviderModelsResponse>(`/providers/${id}/models`),
  usage: (id: string) =>
    apiRequest<ProviderUsage>(`/providers/${id}/usage`),
};

// ---- provider catalog (server-side registered providers) ----
export const providerCatalogApi = {
  list: () => apiRequest<ProviderCatalogEntry[]>("/provider-catalog"),
};

// ---- model catalog ----
export const modelCatalogApi = {
  status: () => apiRequest<ModelCatalogStatus>("/model-catalog"),
  resolve: (body: ModelCatalogResolveRequest) =>
    apiRequest<ModelMetadata>("/model-catalog/resolve", {
      method: "POST",
      body,
    }),
  refresh: () =>
    apiRequest<ModelCatalogStatus>("/model-catalog/refresh", {
      method: "POST",
    }),
};

// ---- routes ----
export interface RouteFilter {
  limit?: number;
  offset?: number;
}
export const routesApi = {
  list: (filter: RouteFilter = {}) =>
    apiRequest<RouteListResponse>("/routes", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  get: (id: string) => apiRequest<Route>(`/routes/${id}`),
  create: (body: RouteInput) =>
    apiRequest<Route>("/routes", { method: "POST", body }),
  update: (id: string, body: RouteInput) =>
    apiRequest<Route>(`/routes/${id}`, { method: "PUT", body }),
  remove: (id: string) =>
    apiRequest<void>(`/routes/${id}`, { method: "DELETE", allowEmpty: true }),
};

// ---- api keys ----
export const apiKeysApi = {
  list: () => apiRequest<ApiKey[]>("/api-keys"),
  get: (id: string) => apiRequest<ApiKeyDetail>(`/api-keys/${id}`),
  create: (body: { name: string; secret?: string; quota?: QuotaSpec }) =>
    apiRequest<CreateApiKeyResponse>("/api-keys", { method: "POST", body }),
  updateQuota: (id: string, quota: QuotaSpec) =>
    apiRequest<ApiKey>(`/api-keys/${id}`, { method: "PATCH", body: { quota } }),
  disable: (id: string) =>
    apiRequest<void>(`/api-keys/${id}`, { method: "PUT", allowEmpty: true }),
  remove: (id: string) =>
    apiRequest<void>(`/api-keys/${id}`, { method: "DELETE", allowEmpty: true }),
};

// ---- oauth ----
export const oauthApi = {
  start: (providerId: string) =>
    apiRequest<OAuthStartResponse>("/oauth/start", {
      method: "POST",
      body: { provider_id: providerId },
    }),
  callback: (code: string, state: string) =>
    apiRequest<OAuthTokenResponse>("/oauth/callback", {
      method: "POST",
      body: { code, state },
    }),
  refresh: (providerId: string) =>
    apiRequest<OAuthTokenResponse>("/oauth/refresh", {
      method: "POST",
      body: { provider_id: providerId },
    }),
};

// ---- stats ----
type StatsRange = { since?: string; until?: string };
export const statsApi = {
  byModel: (range: StatsRange = {}) =>
    apiRequest<StatsResponse>("/stats/by-model", { query: range }),
  byProvider: (range: StatsRange = {}) =>
    apiRequest<StatsResponse>("/stats/by-provider", { query: range }),
  byApiKey: (range: StatsRange = {}) =>
    apiRequest<StatsResponse>("/stats/by-api-key", { query: range }),
  byTarget: (range: StatsRange = {}) =>
    apiRequest<StatsResponse>("/stats/by-target", { query: range }),
  tokenActivity: (days = 365) =>
    apiRequest<TokenActivityResponse>("/stats/token-activity", {
      query: { days },
    }),
  tokenSummary: () => apiRequest<TokenSummaryData>("/stats/token-summary"),
};

// ---- requests (drill-down + replay) ----
export interface RequestFilter {
  request_id?: string;
  since?: string;
  until?: string;
  model?: string;
  provider?: string;
  status?: string;
  error_class?: string;
  min_latency_ms?: number;
  max_latency_ms?: number;
  limit?: number;
  offset?: number;
}
export const requestsApi = {
  list: (filter: RequestFilter = {}) =>
    apiRequest<RequestListResponse>("/requests", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  filterOptions: (filter: Pick<RequestFilter, "since" | "until"> = {}) =>
    apiRequest<RequestFilterOptions>("/requests/filter-options", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  replay: (id: string) => apiRequest<RequestReplay>(`/requests/${id}/replay`),
};

// ---- audit ----
export interface AuditFilter {
  limit?: number;
  offset?: number;
}
export const auditApi = {
  list: (filter: AuditFilter = {}) =>
    apiRequest<AuditListResponse>("/audit", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
};

// ---- health ----
export const healthApi = {
  circuitBreakers: () =>
    apiRequest<CircuitBreakersResponse>("/health/circuit-breakers"),
};

// ---- server info ----
export const serverInfoApi = {
  get: () => apiRequest<ServerInfo>("/info"),
};

// ---- config export / import ----
export const configApi = {
  export: () => apiRequest<ConfigExport>("/config/export"),
  import: (
    masterKey: string,
    config: ConfigExport,
    selection: ImportSelection,
  ) =>
    apiRequest<ImportReport>("/config/import", {
      method: "POST",
      body: { master_key: masterKey, config, selection },
    }),
};

// ---- settings ----
export const settingsApi = {
  list: () => apiRequest<SettingsResponse>("/settings"),
  update: (settings: Settings) =>
    apiRequest<SettingsResponse>("/settings", {
      method: "PUT",
      body: { settings },
    }),
};
