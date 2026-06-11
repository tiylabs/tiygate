import { apiRequest } from "./client";
import type {
  ApiKey,
  ApiKeyDetail,
  AuditEntry,
  CircuitBreakersResponse,
  CreateApiKeyResponse,
  OAuthStartResponse,
  OAuthTokenResponse,
  Provider,
  ProviderInput,
  QuotaSpec,
  RequestListResponse,
  RequestReplay,
  Route,
  RouteInput,
  StatsResponse,
} from "./types";

// ---- providers ----
export const providersApi = {
  list: () => apiRequest<Provider[]>("/providers"),
  get: (id: string) => apiRequest<Provider>(`/providers/${id}`),
  create: (body: ProviderInput) =>
    apiRequest<Provider>("/providers", { method: "POST", body }),
  update: (id: string, body: ProviderInput) =>
    apiRequest<Provider>(`/providers/${id}`, { method: "PUT", body }),
  remove: (id: string) =>
    apiRequest<void>(`/providers/${id}`, { method: "DELETE", allowEmpty: true }),
};

// ---- routes ----
export const routesApi = {
  list: () => apiRequest<Route[]>("/routes"),
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
  create: (body: {
    name: string;
    secret?: string;
    quota?: QuotaSpec;
    tenant_id?: string | null;
  }) => apiRequest<CreateApiKeyResponse>("/api-keys", { method: "POST", body }),
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
};

// ---- requests (drill-down + replay) ----
export interface RequestFilter {
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
  replay: (id: string) =>
    apiRequest<RequestReplay>(`/requests/${id}/replay`),
};

// ---- audit ----
export const auditApi = {
  list: (limit = 100) =>
    apiRequest<AuditEntry[]>("/audit", { query: { limit } }),
};

// ---- health ----
export const healthApi = {
  circuitBreakers: () =>
    apiRequest<CircuitBreakersResponse>("/health/circuit-breakers"),
};
