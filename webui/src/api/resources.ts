import { apiRequest, newIdempotencyKey } from "./client";
import type {
  ApiKey,
  ApiKeyDetail,
  AuditListResponse,
  CapabilityListResponse,
  CapabilityRouteAdmission,
  CapabilityRouteAdmissionListResponse,
  CapabilityMetricsResponse,
  CapabilityProfileResponse,
  CapabilityRequirement,
  CapabilityRegistryEntry,
  CapabilityState,
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
  ProviderResetCreditsConsumeResponse,
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
  consumeResetCredits: (id: string, redeemRequestId: string) =>
    apiRequest<ProviderResetCreditsConsumeResponse>(
      `/providers/${id}/usage/reset-credits`,
      {
        method: "POST",
        body: { redeem_request_id: redeemRequestId },
      },
    ),
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
    apiRequest<Route>("/routes", {
      method: "POST",
      body,
      headers: { "Idempotency-Key": newIdempotencyKey() },
    }),
  update: (id: string, body: RouteInput) =>
    apiRequest<Route>(`/routes/${id}`, {
      method: "PUT",
      body,
      headers: { "Idempotency-Key": newIdempotencyKey() },
    }),
  remove: (id: string) =>
    apiRequest<void>(`/routes/${id}`, {
      method: "DELETE",
      allowEmpty: true,
      headers: { "Idempotency-Key": newIdempotencyKey() },
    }),
  listAll: async () => {
    const entries: Route[] = [];
    let offset = 0;
    for (let page = 0; page < 100; page += 1) {
      const response = await routesApi.list({ limit: 500, offset });
      entries.push(...(response.entries ?? response.items ?? []));
      if (!response.next_cursor) break;
      const next = Number(response.next_cursor);
      if (!Number.isSafeInteger(next) || next <= offset) break;
      offset = next;
    }
    return { total: entries.length, limit: 500, offset: 0, entries, items: entries };
  },
};

// ---- target capabilities ----
export const capabilitiesApi = {
  list: (filter: { limit?: number; offset?: number } = {}) =>
    apiRequest<CapabilityListResponse>("/target-capabilities", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  listAll: async () => {
    const entries: import("./types").CapabilityProfileSummary[] = [];
    let offset = 0;
    for (let page = 0; page < 100; page += 1) {
      const response = await capabilitiesApi.list({ limit: 500, offset });
      entries.push(...(response.entries ?? response.items ?? []));
      if (!response.next_cursor) break;
      const next = Number(response.next_cursor);
      if (!Number.isSafeInteger(next) || next <= offset) break;
      offset = next;
    }
    return { total: entries.length, limit: 500, offset: 0, entries, items: entries };
  },
  get: (targetKey: string) =>
    apiRequest<CapabilityProfileResponse>(`/target-capabilities/${targetKey}`),
  probe: (targetKey: string, probeSet?: string[]) =>
    apiRequest<import("./types").ProbeJob>(
      `/target-capabilities/${targetKey}/probe`,
      {
        method: "POST",
        body: probeSet ? { probe_set: probeSet } : {},
        headers: { "Idempotency-Key": newIdempotencyKey() },
      },
    ),
  override: (
    targetKey: string,
    input: {
      capability_id: string;
      state: CapabilityState;
      value?: unknown;
      reason: string;
      expires_at?: string | null;
    },
  ) =>
    apiRequest<import("./types").CapabilityOverride>(
      `/target-capabilities/${targetKey}/overrides`,
      {
        method: "PUT",
        body: input,
        headers: { "Idempotency-Key": newIdempotencyKey() },
      },
    ),
  removeOverride: (targetKey: string, capabilityId: string) =>
    apiRequest<void>(
      `/target-capabilities/${targetKey}/overrides/${encodeURIComponent(capabilityId)}`,
      {
        method: "DELETE",
        allowEmpty: true,
        headers: { "Idempotency-Key": newIdempotencyKey() },
      },
    ),
  registry: (filter: { limit?: number; offset?: number } = {}) =>
    apiRequest<{
      total: number;
      limit: number;
      offset: number;
      next_cursor?: string | null;
      contract_schema_version?: number;
      contract_summary?: Array<[string, number]>;
      items?: CapabilityRegistryEntry[];
      entries: CapabilityRegistryEntry[];
    }>("/capability-registry", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  registryAll: async () => {
    const entries: CapabilityRegistryEntry[] = [];
    let offset = 0;
    for (let page = 0; page < 100; page += 1) {
      const response = await capabilitiesApi.registry({ limit: 500, offset });
      entries.push(...(response.entries ?? response.items ?? []));
      if (!response.next_cursor) break;
      const next = Number(response.next_cursor);
      if (!Number.isSafeInteger(next) || next <= offset) break;
      offset = next;
    }
    return { total: entries.length, limit: 500, offset: 0, entries, items: entries };
  },
  job: (jobId: string) =>
    apiRequest<import("./types").ProbeJob>(`/probe-jobs/${jobId}`),
  probeRuns: (targetKey: string, filter: { limit?: number; offset?: number } = {}) =>
    apiRequest<{
      total: number;
      limit: number;
      offset: number;
      next_cursor?: string | null;
      items?: import("./types").CapabilityProbeRun[];
      entries: import("./types").CapabilityProbeRun[];
    }>(`/target-capabilities/${encodeURIComponent(targetKey)}/probe-runs`, {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  admissions: (routeId: string, filter: { limit?: number; offset?: number } = {}) =>
    apiRequest<CapabilityRouteAdmissionListResponse>(
      `/routes/${encodeURIComponent(routeId)}/capability-admissions`,
      { query: filter as Record<string, string | number | boolean | undefined> },
    ),
  admissionsAll: async (routeId: string) => {
    const entries: CapabilityRouteAdmission[] = [];
    let offset = 0;
    for (let page = 0; page < 100; page += 1) {
      const response = await capabilitiesApi.admissions(routeId, { limit: 500, offset });
      entries.push(...(response.entries ?? response.items ?? []));
      if (!response.next_cursor) break;
      const next = Number(response.next_cursor);
      if (!Number.isSafeInteger(next) || next <= offset) break;
      offset = next;
    }
    return { route_id: routeId, total: entries.length, limit: 500, offset: 0, entries, items: entries };
  },
  upsertAdmission: (
    routeId: string,
    input: {
      shape_hash?: string;
      required_capabilities: string[];
      required_requirements?: CapabilityRequirement[];
      mode: "shadow" | "enforce";
      expected_revision?: number;
      expires_at?: string | null;
      low_traffic_exception?: boolean;
      reason: string;
    },
  ) =>
    apiRequest<CapabilityRouteAdmission>(
      `/routes/${encodeURIComponent(routeId)}/capability-admissions`,
      {
        method: "POST",
        body: input,
        headers: { "Idempotency-Key": newIdempotencyKey() },
      },
    ),
  removeAdmission: (routeId: string, shapeHash: string, expectedRevision?: number) =>
    apiRequest<void>(
      `/routes/${encodeURIComponent(routeId)}/capability-admissions/${encodeURIComponent(shapeHash)}`,
      {
        method: "DELETE",
        allowEmpty: true,
        headers: { "Idempotency-Key": newIdempotencyKey() },
        query: { expected_revision: expectedRevision },
      },
    ),
  metrics: (filter: {
    route_id?: string;
    shape_hash?: string;
    since?: string;
    until?: string;
    limit?: number;
    offset?: number;
  } = {}) =>
    apiRequest<CapabilityMetricsResponse>("/capability-metrics", {
      query: filter as Record<string, string | number | boolean | undefined>,
    }),
  metricsAll: async (filter: {
    route_id?: string;
    shape_hash?: string;
    since?: string;
    until?: string;
  } = {}) => {
    const entries: import("./types").CapabilityShadowMetric[] = [];
    let offset = 0;
    for (let page = 0; page < 100; page += 1) {
      const response = await capabilitiesApi.metrics({ ...filter, limit: 500, offset });
      entries.push(...response.entries);
      if (!response.next_cursor) break;
      const next = Number(response.next_cursor);
      if (!Number.isSafeInteger(next) || next <= offset) break;
      offset = next;
    }
    return { total: entries.length, limit: 500, offset: 0, entries, items: entries };
  },
  setProbeWorker: (enabled: boolean, reason: string) =>
    apiRequest<{ enabled: boolean }>("/capability-probes", {
      method: "PUT",
      body: { enabled, reason },
      headers: { "Idempotency-Key": newIdempotencyKey() },
    }),
};

// ---- api keys ----
export const apiKeysApi = {
  list: () => apiRequest<ApiKey[]>("/api-keys"),
  get: (id: string) => apiRequest<ApiKeyDetail>(`/api-keys/${id}`),
  create: (body: {
    name: string;
    secret?: string;
    quota?: QuotaSpec;
    allowed_models?: string[] | null;
  }) =>
    apiRequest<CreateApiKeyResponse>("/api-keys", { method: "POST", body }),
  updateQuota: (id: string, quota: QuotaSpec) =>
    apiRequest<ApiKey>(`/api-keys/${id}`, { method: "PATCH", body: { quota } }),
  updateModelAccess: (id: string, allowedModels: string[] | null) =>
    apiRequest<ApiKey>(`/api-keys/${id}/model-access`, {
      method: "PATCH",
      body: { allowed_models: allowedModels },
    }),
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
      headers: { "Idempotency-Key": newIdempotencyKey() },
    }),
};

// ---- settings ----
export const settingsApi = {
  list: () => apiRequest<SettingsResponse>("/settings"),
  update: (settings: Settings) =>
    apiRequest<SettingsResponse>("/settings", {
      method: "PUT",
      body: { settings },
      headers: { "Idempotency-Key": newIdempotencyKey() },
    }),
};
