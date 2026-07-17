import { useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  useMutation,
  useQueries,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  Plus,
  Pencil,
  Trash2,
  ExternalLink,
  RefreshCw,
  Play,
  Copy,
} from "lucide-react";
import { providersApi, providerCatalogApi, oauthApi } from "@/api/resources";
import type {
  Provider,
  ProviderDeleteImpact,
  ProviderInput,
  ProviderUsage,
  ProviderUsageWindow,
} from "@/api/types";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Dialog,
  EmptyState,
  ErrorBox,
  Field,
  Input,
  PasswordInput,
  RowActions,
  Select,
  Switch,
  Table,
  TableSkeleton,
  Thead,
  Td,
  Th,
  Tr,
  Alert,
  useStickyTableScroll,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { cn } from "@/lib/cn";
import { parseCallbackUrl } from "@/lib/oauth";
import { openExternalUrl } from "@/lib/external-url";
import { VendorIcon } from "@/lib/vendors";

const AUTH_MODES = ["api_key", "oauth"];
const OPENAI_PLATFORM_BASE_URL = "https://api.openai.com/v1";
const OPENAI_CODEX_BASE_URL = "https://chatgpt.com/backend-api/codex";

/** Vendors that have a built-in OAuth preset (crates/auth/src/provider_oauth.rs). */
const OAUTH_VENDORS = new Set(["openai", "anthropic"]);

/**
 * Refresh metadata embedded into `metadata_json["oauth"]` when a provider is
 * saved with auth_mode=oauth. Authorization scopes remain backend-owned.
 */
const OAUTH_PRESETS: Record<
  string,
  {
    token_url: string;
    client_id: string;
    scopes: string[];
    token_request_style: "form" | "json";
  }
> = {
  openai: {
    token_url: "https://auth.openai.com/oauth/token",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scopes: ["openid", "profile", "email"],
    token_request_style: "form",
  },
  anthropic: {
    token_url: "https://api.anthropic.com/v1/oauth/token",
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    scopes: [
      "user:profile",
      "user:inference",
      "user:sessions:claude_code",
      "user:mcp_servers",
      "user:file_upload",
    ],
    token_request_style: "json",
  },
};

function authModeLabelKey(mode: string): string {
  return mode === "oauth"
    ? "providers.authModes.oauth"
    : "providers.authModes.staticKey";
}

interface FormState {
  id?: string;
  name: string;
  vendor: string;
  api_base: string;
  models_endpoint: string;
  api_key: string;
  auth_mode: string;
  enabled: boolean;
}

interface PendingDeleteState {
  provider: Provider;
  impact?: ProviderDeleteImpact;
  loading: boolean;
  error?: string;
}

function emptyForm(): FormState {
  return {
    name: "",
    vendor: "openai",
    api_base: "",
    models_endpoint: "",
    api_key: "",
    auth_mode: "api_key",
    enabled: true,
  };
}

function hasOAuthMeta(provider: Provider | null): boolean {
  const meta = provider?.encrypted_oauth_meta?.trim() ?? "";
  return meta !== "" && meta !== "[encrypted: <short>]";
}

function oauthStatusTone(
  provider: Provider,
): "success" | "warning" | "danger" | "info" | "neutral" {
  switch (provider.oauth_status?.state) {
    case "healthy":
      return "success";
    case "invalid":
      return "danger";
    case "error":
      return "warning";
    case "connected":
      return "info";
    default:
      return "neutral";
  }
}

function oauthStatusKey(provider: Provider): string {
  return `providers.oauthStatus.${provider.oauth_status?.state ?? "not_connected"}`;
}

function supportsOAuthUsage(provider: Provider): boolean {
  return (
    provider.auth_mode === "oauth" &&
    (provider.vendor === "openai" || provider.vendor === "anthropic")
  );
}

function isOAuthProvider(provider: Provider): boolean {
  return provider.auth_mode === "oauth";
}

const PLAN_TYPE_LABELS: Record<string, string> = {
  free: "Free",
  go: "Go",
  plus: "Plus",
  pro: "Pro",
  pro_lite: "Pro Lite",
  max: "Max",
  team: "Team",
  default_claude_max_5x: "Max 5x",
  default_claude_max_20x: "Max 20x",
  business: "Business",
  enterprise: "Enterprise",
  education: "Education",
  edu: "Education",
};

function formatPlanType(planType: string | null | undefined): string | null {
  const normalized = planType?.trim().toLowerCase();
  if (!normalized) return null;
  return PLAN_TYPE_LABELS[normalized] ?? planType?.trim() ?? null;
}

const FIVE_HOURS_SECONDS = 5 * 60 * 60;
const SEVEN_DAYS_SECONDS = 7 * 24 * 60 * 60;

function providerUsageWindows(
  usage: ProviderUsage | undefined,
): ProviderUsageWindow[] {
  if (Array.isArray(usage?.windows)) return usage.windows;

  // Backward compatibility while the WebUI may be served by an older Admin
  // API during rolling upgrades.
  const windows: ProviderUsageWindow[] = [];
  if (usage?.five_hour) {
    windows.push({
      ...usage.five_hour,
      limit_window_seconds:
        usage.five_hour.limit_window_seconds ?? FIVE_HOURS_SECONDS,
    });
  }
  if (usage?.seven_day) {
    windows.push({
      ...usage.seven_day,
      limit_window_seconds:
        usage.seven_day.limit_window_seconds ?? SEVEN_DAYS_SECONDS,
    });
  }
  return windows;
}

function formatUsageWindowLabel(
  window: ProviderUsageWindow | undefined,
  index: number,
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  const explicitLabel = window?.label?.trim();
  if (explicitLabel) return explicitLabel;

  const seconds = window?.limit_window_seconds;
  if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds <= 0) {
    return t("providers.usage.windowFallback", { index: index + 1 });
  }

  const formatAmount = (amount: number) =>
    Number.isInteger(amount)
      ? amount.toString()
      : amount.toFixed(1).replace(/\.0$/, "");
  if (seconds >= 24 * 60 * 60) {
    return `${formatAmount(seconds / (24 * 60 * 60))}d`;
  }
  if (seconds >= 60 * 60) {
    return `${formatAmount(seconds / (60 * 60))}h`;
  }
  return `${formatAmount(seconds / 60)}m`;
}

function formatUsageResetTime(
  resetAt: number | null | undefined,
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  if (resetAt == null) return "—";
  const remainingMinutes = Math.max(
    1,
    Math.ceil((resetAt * 1000 - Date.now()) / 60_000),
  );
  const days = Math.floor(remainingMinutes / (24 * 60));
  const hours = Math.floor((remainingMinutes % (24 * 60)) / 60);
  const minutes = remainingMinutes % 60;

  if (days > 0) {
    return hours > 0
      ? t("providers.usage.resetAfter.daysHours", { days, hours })
      : t("providers.usage.resetAfter.days", { days });
  }
  if (hours > 0) {
    return minutes > 0
      ? t("providers.usage.resetAfter.hoursMinutes", { hours, minutes })
      : t("providers.usage.resetAfter.hours", { hours });
  }
  return t("providers.usage.resetAfter.minutes", { minutes });
}

function usageWindowPercent(window: ProviderUsageWindow | null | undefined) {
  const value = window?.used_percent;
  return typeof value === "number" && Number.isFinite(value)
    ? Math.min(100, Math.max(0, value))
    : null;
}

function UsageWindow({
  label,
  window,
  loading,
  state,
  wide,
  t,
}: {
  label: string;
  window?: ProviderUsageWindow | null;
  loading: boolean;
  state?: ProviderUsage["state"];
  wide?: boolean;
  t: (key: string, options?: Record<string, unknown>) => string;
}) {
  const percent = usageWindowPercent(window);
  const isAvailable = state === "available" && percent != null;
  const resetAt = formatUsageResetTime(window?.reset_at, t);

  return (
    <div className={cn("min-w-0", wide && "col-span-2")}>
      <div className="mb-0.5 flex min-w-0 items-center gap-1 text-[10px]">
        <span>{label}</span>
        <span
          className="min-w-0 flex-1 truncate text-[9px] font-normal normal-case tracking-normal text-text-subtle"
          title={isAvailable ? resetAt : undefined}
        >
          {loading
            ? "…"
            : isAvailable
              ? resetAt
              : state === "not_connected"
                ? t("providers.usage.notConnected")
                : t("providers.usage.unavailable")}
        </span>
        {loading ? (
          <span className="text-text-subtle">…</span>
        ) : isAvailable ? (
          <span className="font-mono normal-case text-text">
            {percent.toFixed(0)}%
          </span>
        ) : (
          <span className="font-mono normal-case text-text-subtle">—</span>
        )}
      </div>
      <div className="h-1 overflow-hidden rounded-full bg-surface-muted">
        {loading ? (
          <div className="h-full w-1/2 animate-pulse rounded-full bg-border-strong" />
        ) : isAvailable ? (
          <div
            className={cn(
              "h-full rounded-full transition-[width] duration-300",
              percent >= 90 ? "bg-danger" : "bg-primary",
            )}
            style={{ width: `${percent}%` }}
          />
          ) : null}
      </div>
    </div>
  );
}

export default function Providers() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();
  const deleteImpactRequestRef = useRef(0);
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
    refetchInterval: 30_000,
  });
  const {
    data: catalog,
    isLoading: catalogLoading,
    isError: catalogError,
  } = useQuery({
    queryKey: ["provider-catalog"],
    queryFn: providerCatalogApi.list,
  });
  const usageQueries = useQueries({
    queries: (data ?? []).map((provider) => ({
      queryKey: ["provider-usage", provider.id],
      queryFn: () => providersApi.usage(provider.id),
      enabled: supportsOAuthUsage(provider),
      staleTime: 60_000,
      retry: false,
    })),
  });
  const { scrollRef, scrollState } = useStickyTableScroll([
    isLoading,
    data?.length ?? 0,
  ]);

  // Map catalog id → display name for the table's vendor column.
  const catalogLabel = useMemo(() => {
    const m = new Map<string, string>();
    for (const e of catalog ?? []) m.set(e.id, e.display_name);
    return m;
  }, [catalog]);
  const usageByProvider = useMemo(() => {
    const result = new Map<string, (typeof usageQueries)[number]>();
    for (const [index, provider] of (data ?? []).entries()) {
      result.set(provider.id, usageQueries[index]);
    }
    return result;
  }, [data, usageQueries]);
  const [modalOpen, setModalOpen] = useState(false);
  const [form, setForm] = useState<FormState>(emptyForm());
  const [editing, setEditing] = useState<Provider | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<PendingDeleteState | null>(
    null,
  );

  // OAuth flow state (used inside the edit dialog when auth_mode=oauth).
  const [oauthAuthUrl, setOauthAuthUrl] = useState<string | null>(null);
  const [oauthState, setOauthState] = useState<string | null>(null);
  const [oauthCallbackUrl, setOauthCallbackUrl] = useState("");
  const [oauthMessage, setOauthMessage] = useState<string | null>(null);
  const [oauthError, setOauthError] = useState<string | null>(null);

  // Options for the vendor dropdown, sourced from the server catalog. When
  // editing a provider whose vendor is no longer in the catalog (server
  // narrowed the set), we inject the current value so its value is never
  // silently dropped.
  const vendorOptions = useMemo(() => {
    const entries = catalog ?? [];
    const opts = entries.map((e) => ({
      value: e.id,
      label: (
        <span className="flex items-center gap-2">
          <VendorIcon vendor={e.id} className="h-4 w-4" />
          <span>{e.display_name}</span>
        </span>
      ),
    }));
    if (form.vendor && !entries.some((e) => e.id === form.vendor)) {
      opts.push({
        value: form.vendor,
        label: (
          <span className="flex items-center gap-2">
            <VendorIcon vendor={form.vendor} className="h-4 w-4" />
            <span>{form.vendor}</span>
          </span>
        ),
      });
    }
    return opts;
  }, [catalog, form.vendor]);

  const invalidateProviders = () =>
    qc.invalidateQueries({ queryKey: ["providers"] });
  const invalidateProviderDelete = () => {
    void qc.invalidateQueries({ queryKey: ["providers"] });
    void qc.invalidateQueries({ queryKey: ["routes"] });
  };

  const saveMutation = useMutation({
    mutationFn: (input: { id?: string; body: ProviderInput }) =>
      input.id
        ? providersApi.update(input.id, input.body)
        : providersApi.create(input.body),
    onSuccess: (savedProvider: Provider) => {
      const shouldKeepOpenForOAuth =
        savedProvider.auth_mode === "oauth" && !hasOAuthMeta(savedProvider);
      if (shouldKeepOpenForOAuth) {
        // Keep the dialog open only when OAuth still needs authorization.
        // Once encrypted_oauth_meta exists, saving behaves like normal edits.
        setEditing(savedProvider);
        setForm((prev) => ({ ...prev, id: savedProvider.id }));
        setFormError(null);
      } else {
        setModalOpen(false);
      }
      toast.success(t("providers.saved"));
      void invalidateProviders();
    },
    onError: (e: Error) => setFormError(e.message),
  });

  const oauthStartMutation = useMutation({
    mutationFn: () => oauthApi.start(editing!.id),
    onSuccess: (res) => {
      setOauthError(null);
      setOauthAuthUrl(res.url);
      setOauthState(res.state);
      setOauthCallbackUrl("");
      setOauthMessage(t("oauth.started"));
    },
    onError: (e: Error) => {
      setOauthError(e.message);
      setOauthAuthUrl(null);
      setOauthMessage(null);
    },
  });

  const oauthCallbackMutation = useMutation({
    mutationFn: () => {
      const parsed = parseCallbackUrl(
        oauthCallbackUrl,
        oauthState ?? undefined,
      );
      if (!parsed) {
        throw new Error(t("oauth.callbackUrlInvalid"));
      }
      return oauthApi.callback(parsed.code, parsed.state);
    },
    onSuccess: (res) => {
      setOauthError(null);
      const label = `${editing?.name ?? ""} (${res.provider_id})`;
      setOauthMessage(t("oauth.callbackSuccess", { provider: label }));
      toast.success(t("oauth.callbackSuccess", { provider: label }));
      setOauthAuthUrl(null);
      setOauthState(null);
      setOauthCallbackUrl("");
      // Refresh provider data so encrypted_oauth_meta is up to date.
      void invalidateProviders();
      void providersApi
        .get(res.provider_id)
        .then((p) => setEditing(p))
        .catch(() => {
          /* leave editing as-is; list refetch covers the table */
        });
    },
    onError: (e: Error) => {
      setOauthError(e.message);
      setOauthMessage(null);
    },
  });

  const oauthRefreshMutation = useMutation({
    mutationFn: () => oauthApi.refresh(editing!.id),
    onSuccess: (res) => {
      setOauthError(null);
      const label = `${editing?.name ?? ""} (${res.provider_id})`;
      setOauthMessage(t("oauth.refreshed", { provider: label }));
      toast.success(t("oauth.refreshed", { provider: label }));
      void invalidateProviders();
      void providersApi.get(res.provider_id).then(setEditing);
    },
    onError: (e: Error) => {
      setOauthError(e.message);
      setOauthMessage(null);
      void invalidateProviders();
      if (editing) void providersApi.get(editing.id).then(setEditing);
    },
  });

  async function copyOauthUrl() {
    if (!oauthAuthUrl) return;
    try {
      await navigator.clipboard.writeText(oauthAuthUrl);
      toast.success(t("oauth.urlCopied"));
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }

  async function openOauthUrl() {
    if (!oauthAuthUrl) return;
    const opened = await openExternalUrl(oauthAuthUrl);
    if (!opened) await copyOauthUrl();
  }

  /** Reset all OAuth dialog state when the dialog opens/closes. */
  function resetOauthState() {
    setOauthAuthUrl(null);
    setOauthCallbackUrl("");
    setOauthMessage(null);
    setOauthError(null);
  }

  const deleteMutation = useMutation({
    mutationFn: providersApi.remove,
    onSuccess: () => {
      setPendingDelete(null);
      toast.success(t("providers.deleted"));
      invalidateProviderDelete();
    },
    onError: (e: Error) => {
      setPendingDelete(null);
      toast.error(t("providers.deleteFailed"), e.message);
    },
  });

  function openCreate() {
    setEditing(null);
    setForm(emptyForm());
    setFormError(null);
    resetOauthState();
    setModalOpen(true);
  }

  function openEdit(p: Provider) {
    setEditing(p);
    setForm({
      id: p.id,
      name: p.name,
      vendor: p.vendor,
      api_base: p.api_base,
      models_endpoint: p.models_endpoint,
      api_key: "",
      auth_mode: p.auth_mode,
      enabled: p.enabled,
    });
    setFormError(null);
    resetOauthState();
    setModalOpen(true);
  }

  async function openDelete(p: Provider) {
    const requestId = deleteImpactRequestRef.current + 1;
    deleteImpactRequestRef.current = requestId;
    setPendingDelete({ provider: p, loading: true });
    try {
      const impact = await providersApi.deleteImpact(p.id);
      setPendingDelete((current) =>
        deleteImpactRequestRef.current === requestId &&
        current?.provider.id === p.id
          ? { provider: p, impact, loading: false }
          : current,
      );
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (deleteImpactRequestRef.current === requestId) {
        toast.error(t("providers.deleteImpactLoadFailed"), message);
      }
      setPendingDelete((current) =>
        deleteImpactRequestRef.current === requestId &&
        current?.provider.id === p.id
          ? { provider: p, loading: false, error: message }
          : current,
      );
    }
  }

  function submit() {
    setFormError(null);
    const isOAuth = form.auth_mode === "oauth";
    const isOpenAi = form.vendor === "openai";
    const apiBase = isOpenAi
      ? isOAuth
        ? !form.api_base || form.api_base === OPENAI_PLATFORM_BASE_URL
          ? OPENAI_CODEX_BASE_URL
          : form.api_base
        : !form.api_base || form.api_base === OPENAI_CODEX_BASE_URL
          ? OPENAI_PLATFORM_BASE_URL
          : form.api_base
      : form.api_base;
    const modelsEndpoint = isOpenAi
      ? isOAuth
        ? !form.models_endpoint ||
          form.models_endpoint === `${OPENAI_PLATFORM_BASE_URL}/models`
          ? `${OPENAI_CODEX_BASE_URL}/models`
          : form.models_endpoint
        : !form.models_endpoint ||
            form.models_endpoint === `${OPENAI_CODEX_BASE_URL}/models`
          ? `${OPENAI_PLATFORM_BASE_URL}/models`
          : form.models_endpoint
      : form.models_endpoint;
    const body: ProviderInput = {
      name: form.name,
      vendor: form.vendor,
      api_base: apiBase,
      models_endpoint: modelsEndpoint,
      auth_mode: form.auth_mode,
      enabled: form.enabled,
    };
    // Only send api_key when the operator typed one — blank keeps the
    // existing encrypted secret untouched.
    if (form.api_key.trim()) body.api_key = form.api_key.trim();

    // For OAuth providers, embed the OAuth preset metadata so the
    // backend's snapshot_to_routing_table can build OAuthTargetConfig
    // (token_url, client_id, scopes, etc.) from metadata_json["oauth"].
    if (isOAuth) {
      const oauthConfig = OAUTH_PRESETS[form.vendor];
      if (oauthConfig) {
        body.metadata = { oauth: oauthConfig };
      }
    }

    saveMutation.mutate({ id: editing?.id, body });
  }

  function renderDeleteDescription(): ReactNode {
    if (!pendingDelete) return null;
    const { provider, impact, loading, error } = pendingDelete;
    return (
      <div className="space-y-2">
        <p>{t("providers.deleteConfirm", { name: provider.name })}</p>
        {loading ? <p>{t("providers.deleteImpactLoading")}</p> : null}
        {error ? <p>{t("providers.deleteImpactLoadFailed")}</p> : null}
        {impact && impact.route_count > 0 ? (
          <>
            <p>
              {t("providers.deleteImpactRoutes", {
                count: impact.route_count,
                targets: impact.target_count,
              })}
            </p>
            {impact.delete_route_count > 0 ? (
              <p>
                {t("providers.deleteImpactEmptyRoutes", {
                  count: impact.delete_route_count,
                })}
              </p>
            ) : null}
          </>
        ) : null}
      </div>
    );
  }

  return (
    <div>
      <PageHeader
        title={t("providers.title")}
        action={
          <Button
            variant="primary"
            icon={<Plus size={16} />}
            onClick={openCreate}
          >
            {t("providers.add")}
          </Button>
        }
      />
      {error ? (
        <ErrorBox
          message={(error as Error).message}
          onRetry={() => refetch()}
          retryLabel={t("common.retry")}
        />
      ) : (
        <Card>
          {isLoading ? (
            <TableSkeleton rowHeight="h-14" />
          ) : (data ?? []).length === 0 ? (
            <EmptyState
              title={t("common.emptyTitle")}
              description={t("providers.empty")}
              action={
                <Button
                  variant="primary"
                  icon={<Plus size={16} />}
                  onClick={openCreate}
                >
                  {t("providers.add")}
                </Button>
              }
            />
          ) : (
            <Table
              maxHeight={[
                "max-h-[calc(100vh-9.5rem)]",
                "lg:max-h-[calc(100vh-5.5rem)]",
              ]}
              tableClassName="min-w-max border-separate border-spacing-0"
              containerRef={scrollRef}
            >
              <colgroup>
                <col style={{ width: "20rem" }} />
                <col style={{ width: "16%" }} />
                <col />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "9rem" }} />
                <col style={{ width: "3.5rem" }} />
              </colgroup>
              <Thead>
                <tr>
                  <Th
                    className={cn(
                      "sticky left-0 z-30 w-80 bg-surface-muted",
                      scrollState !== "start" &&
                        "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("common.name")}
                  </Th>
                  <Th>{t("providers.vendor")}</Th>
                  <Th>{t("providers.apiBase")}</Th>
                  <Th>{t("providers.authMode")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.updatedAt")}</Th>
                  <Th
                    className={cn(
                      "sticky right-0 z-30 bg-surface-muted text-right",
                      scrollState !== "end" &&
                        "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("common.actions")}
                  </Th>
                </tr>
              </Thead>
              <tbody>
                {(data ?? []).map((p) => (
                  <Tr key={p.id}>
                    <Td
                      className={cn(
                        "sticky left-0 z-10 w-80 bg-surface align-middle group-hover:bg-surface-muted",
                        scrollState !== "start" &&
                          "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
                      <div
                        className="truncate font-medium text-text"
                        title={p.name}
                      >
                        {p.name}
                      </div>
                      <div
                        className="break-all font-mono text-xs text-text-subtle"
                        title={p.id}
                      >
                        {p.id}
                      </div>
                    </Td>
                    <Td className="align-middle">
                      <div className="flex items-center gap-2">
                        <span className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-primary-soft text-primary">
                          <VendorIcon vendor={p.vendor} />
                        </span>
                        <span className="truncate">
                          {catalogLabel.get(p.vendor) ?? p.vendor}
                        </span>
                      </div>
                    </Td>
                    <Td className="align-middle">
                      {!isOAuthProvider(p) ? (
                        <div
                          className="truncate font-mono text-xs"
                          title={p.api_base}
                        >
                          {p.api_base}
                        </div>
                      ) : null}
                      {supportsOAuthUsage(p) ? (
                        <div className="mt-1.5 grid min-w-[16rem] grid-cols-2 gap-x-3 gap-y-1.5">
                          {(() => {
                            const usageQuery = usageByProvider.get(p.id);
                            const usage = usageQuery?.data;
                            const accountEmail = usage?.account_email;
                            const planType = formatPlanType(usage?.plan_type);
                            const windows = providerUsageWindows(usage);
                            const visibleWindows =
                              windows.length > 0 ? windows : [undefined];
                            return (
                              <>
                                <div className="col-span-2 flex min-w-0 items-center gap-1 text-[10px] leading-4">
                                  {planType ? (
                                    <Badge
                                      tone="primary"
                                      className="shrink-0 px-1.5 py-0 text-[10px]"
                                      title={usage?.plan_type ?? undefined}
                                    >
                                      {planType}
                                    </Badge>
                                  ) : null}
                                  <span
                                    className="min-w-0 truncate font-mono text-[10px] text-text-muted"
                                    title={accountEmail ?? undefined}
                                  >
                                    {accountEmail ??
                                      (usageQuery?.isFetching
                                        ? t("providers.usage.loading")
                                        : "—")}
                                  </span>
                                </div>
                                {visibleWindows.map((window, index) => (
                                  <UsageWindow
                                    key={`${window?.label ?? "main"}-${window?.limit_window_seconds ?? "unknown"}-${index}`}
                                    label={formatUsageWindowLabel(window, index, t)}
                                    window={window}
                                    loading={usageQuery?.isFetching ?? true}
                                    state={usage?.state}
                                    wide={visibleWindows.length === 1}
                                    t={t}
                                  />
                                ))}
                              </>
                            );
                          })()}
                        </div>
                      ) : null}
                    </Td>
                    <Td className="whitespace-nowrap text-xs">
                      <div className="flex flex-col items-start gap-1.5">
                        <span>{t(authModeLabelKey(p.auth_mode))}</span>
                        {p.auth_mode === "oauth" ? (
                          <Badge
                            tone={oauthStatusTone(p)}
                            title={
                              p.oauth_status?.checked_at
                                ? t("providers.oauthStatus.checkedAt", {
                                    time: fmtTime(p.oauth_status.checked_at),
                                  })
                                : undefined
                            }
                          >
                            {t(oauthStatusKey(p))}
                          </Badge>
                        ) : null}
                      </div>
                    </Td>
                    <Td className="text-center whitespace-nowrap">
                      {p.enabled ? (
                        <Badge tone="success">{t("common.enabled")}</Badge>
                      ) : (
                        <Badge tone="neutral">{t("common.disabled")}</Badge>
                      )}
                    </Td>
                    <Td className="text-xs text-text-muted whitespace-nowrap">
                      {fmtTime(p.updated_at)}
                    </Td>
                    <Td
                      className={cn(
                        "sticky right-0 z-10 bg-surface text-right group-hover:bg-surface-muted",
                        scrollState !== "end" &&
                          "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
                      <RowActions
                        label={t("common.rowActions")}
                        items={[
                          {
                            key: "edit",
                            label: t("common.edit"),
                            icon: <Pencil size={14} />,
                            onSelect: () => openEdit(p),
                          },
                          {
                            key: "delete",
                            label: t("common.delete"),
                            icon: <Trash2 size={14} />,
                            destructive: true,
                            onSelect: () => void openDelete(p),
                          },
                        ]}
                      />
                    </Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>
      )}

      <Dialog
        open={modalOpen}
        onOpenChange={setModalOpen}
        title={editing ? t("providers.editTitle") : t("providers.addTitle")}
        closeLabel={t("common.close")}
        footer={
          <>
            <Button variant="secondary" onClick={() => setModalOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              onClick={submit}
              loading={saveMutation.isPending}
            >
              {t("common.save")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {formError ? <ErrorBox message={formError} /> : null}
          <Field label={t("common.name")} required>
            <Input
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
            />
          </Field>
          <Field label={t("providers.vendor")}>
            <Select
              value={form.vendor}
              onValueChange={(v) => {
                setForm((prev) => {
                  // If switching to a vendor that doesn't support OAuth
                  // while auth_mode is "oauth", reset to "api_key".
                  const authMode =
                    prev.auth_mode === "oauth" && !OAUTH_VENDORS.has(v)
                      ? "api_key"
                      : prev.auth_mode;
                  return {
                    ...prev,
                    vendor: v,
                    auth_mode: authMode,
                  };
                });
              }}
              ariaLabel={t("providers.vendor")}
              disabled={
                catalogLoading || catalogError || vendorOptions.length === 0
              }
              placeholder={
                catalogLoading
                  ? t("providers.vendorLoading")
                  : catalogError
                    ? t("providers.vendorLoadError")
                    : undefined
              }
              options={vendorOptions}
            />
          </Field>
          {form.auth_mode !== "oauth" ? (
            <>
              <Field label={t("providers.apiBase")}>
                <Input
                  value={form.api_base}
                  onChange={(e) =>
                    setForm({ ...form, api_base: e.target.value })
                  }
                  placeholder={
                    catalog?.find((e) => e.id === form.vendor)
                      ?.default_base_url ?? ""
                  }
                  onKeyDown={(e) => {
                    if (e.key === "Tab" && !form.api_base) {
                      const entry = catalog?.find((el) => el.id === form.vendor);
                      if (entry?.default_base_url) {
                        e.preventDefault();
                        setForm((prev) => ({
                          ...prev,
                          api_base: entry.default_base_url,
                        }));
                      }
                    }
                  }}
                />
              </Field>
              <Field label={t("providers.modelsEndpoint")}>
                <Input
                  value={form.models_endpoint}
                  onChange={(e) =>
                    setForm({ ...form, models_endpoint: e.target.value })
                  }
                  placeholder={(() => {
                    const base =
                      form.api_base ||
                      catalog?.find((e) => e.id === form.vendor)
                        ?.default_base_url ||
                      "";
                    return base ? base + "/models" : "";
                  })()}
                  onKeyDown={(e) => {
                    if (e.key === "Tab" && !form.models_endpoint) {
                      const base =
                        form.api_base ||
                        catalog?.find((el) => el.id === form.vendor)
                          ?.default_base_url ||
                        "";
                      if (base) {
                        e.preventDefault();
                        setForm((prev) => ({
                          ...prev,
                          models_endpoint: base + "/models",
                        }));
                      }
                    }
                  }}
                />
              </Field>
            </>
          ) : null}
          <Field label={t("providers.authMode")}>
            <Select
              value={form.auth_mode}
              onValueChange={(v) => {
                if (v !== "oauth") resetOauthState();
                setForm({ ...form, auth_mode: v });
              }}
              ariaLabel={t("providers.authMode")}
              options={AUTH_MODES.map((m) => {
                const oauthSupported = OAUTH_VENDORS.has(form.vendor);
                return {
                  value: m,
                  label:
                    m === "oauth" && !oauthSupported
                      ? `${t(authModeLabelKey(m))}（${t("providers.unsupportedVendor")}）`
                      : t(authModeLabelKey(m)),
                  disabled: m === "oauth" && !oauthSupported,
                };
              })}
            />
          </Field>
          {form.auth_mode === "oauth" ? (
            editing ? (
              <div className="space-y-3 rounded-lg border border-border bg-surface-muted/40 p-4">
                <div className="flex items-center justify-between">
                  <span className="text-sm font-medium text-text">
                    {t("providers.oauthPanel.title")}
                  </span>
                  <Badge tone={oauthStatusTone(editing)}>
                    {t(oauthStatusKey(editing))}
                  </Badge>
                </div>
                {editing.oauth_status?.state === "invalid" ? (
                  <Alert tone="danger">
                    {t("providers.oauthStatus.invalidHint")}
                  </Alert>
                ) : null}
                {editing.oauth_status?.state === "error" ? (
                  <Alert tone="warning">
                    {t("providers.oauthStatus.errorHint")}
                  </Alert>
                ) : null}
                {editing.oauth_status?.state === "connected" ? (
                  <Alert tone="info">
                    {t("providers.oauthStatus.connectedHint")}
                  </Alert>
                ) : null}
                <div className="flex flex-wrap gap-2">
                  <Button
                    variant="primary"
                    icon={<Play size={16} />}
                    loading={oauthStartMutation.isPending}
                    onClick={() => oauthStartMutation.mutate()}
                  >
                    {t(
                      editing.oauth_status?.state === "invalid"
                        ? "providers.oauthPanel.reauthorize"
                        : "providers.oauthPanel.start",
                    )}
                  </Button>
                  <Button
                    variant="secondary"
                    icon={<RefreshCw size={16} />}
                    disabled={!hasOAuthMeta(editing)}
                    loading={oauthRefreshMutation.isPending}
                    onClick={() => oauthRefreshMutation.mutate()}
                  >
                    {t("providers.oauthPanel.refresh")}
                  </Button>
                </div>
                {oauthError ? <ErrorBox message={oauthError} /> : null}
                {oauthMessage ? (
                  <Alert tone="success">{oauthMessage}</Alert>
                ) : null}
                {oauthAuthUrl ? (
                  <Field label={t("providers.oauthPanel.authorizeUrl")}>
                    <div className="space-y-2">
                      <code className="block w-full break-all rounded-md bg-surface-muted px-3 py-2 font-mono text-xs text-text">
                        {oauthAuthUrl}
                      </code>
                      <div className="flex flex-wrap gap-2">
                        <Button
                          variant="secondary"
                          icon={<Copy size={14} />}
                          onClick={copyOauthUrl}
                        >
                          {t("providers.oauthPanel.copyUrl")}
                        </Button>
                        <Button
                          variant="accent"
                          icon={<ExternalLink size={14} />}
                          onClick={openOauthUrl}
                        >
                          {t("providers.oauthPanel.openUrl")}
                        </Button>
                      </div>
                    </div>
                  </Field>
                ) : null}
                {oauthAuthUrl ? (
                  <Field label={t("providers.oauthPanel.callbackHint")}>
                    <div className="space-y-2">
                      <textarea
                        className="min-h-[60px] w-full resize-y rounded-md border border-border bg-surface px-3 py-2 font-mono text-xs text-text placeholder:text-text-muted focus:outline-none focus:ring-2 focus:ring-primary/40"
                        placeholder={t(
                          "providers.oauthPanel.callbackUrlPlaceholder",
                        )}
                        value={oauthCallbackUrl}
                        onChange={(e) => setOauthCallbackUrl(e.target.value)}
                      />
                      <Button
                        variant="primary"
                        disabled={!oauthCallbackUrl.trim()}
                        loading={oauthCallbackMutation.isPending}
                        onClick={() => oauthCallbackMutation.mutate()}
                      >
                        {t("providers.oauthPanel.submitCallback")}
                      </Button>
                    </div>
                  </Field>
                ) : null}
              </div>
            ) : (
              <Alert tone="info">{t("providers.oauthPanel.saveFirst")}</Alert>
            )
          ) : null}
          {form.auth_mode !== "oauth" ? (
            <Field
              label={t("providers.apiKey")}
              hint={
                editing ? t("providers.apiKeyHint") : t("providers.redacted")
              }
            >
              <PasswordInput
                value={form.api_key}
                onChange={(e) => setForm({ ...form, api_key: e.target.value })}
                placeholder={editing ? "••••••••" : "sk-…"}
                toggleLabel={t("providers.apiKey")}
                autoComplete="off"
              />
            </Field>
          ) : null}
          <Switch
            checked={form.enabled}
            onCheckedChange={(v) => setForm({ ...form, enabled: v })}
            label={t("common.enabled")}
          />
        </div>
      </Dialog>

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(o) => !o && setPendingDelete(null)}
        title={t("providers.deleteTitle")}
        description={renderDeleteDescription()}
        confirmLabel={t("common.delete")}
        cancelLabel={t("common.cancel")}
        destructive
        loading={deleteMutation.isPending || (pendingDelete?.loading ?? false)}
        onConfirm={() =>
          pendingDelete && deleteMutation.mutate(pendingDelete.provider.id)
        }
      />
    </div>
  );
}
