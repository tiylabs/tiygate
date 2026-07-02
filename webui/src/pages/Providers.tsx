import { useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Pencil, Trash2, ExternalLink, RefreshCw, Play, Copy } from "lucide-react";
import { providersApi, providerCatalogApi, oauthApi } from "@/api/resources";
import type {
  Provider,
  ProviderDeleteImpact,
  ProviderInput,
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

/** Vendors that have a built-in OAuth preset (crates/auth/src/provider_oauth.rs). */
const OAUTH_VENDORS = new Set(["openai", "anthropic", "xai"]);

/**
 * OAuth preset metadata embedded into `metadata_json["oauth"]` when a
 * provider is saved with auth_mode=oauth. Mirrors the values in
 * crates/auth/src/provider_oauth.rs so the backend's
 * `build_oauth_target_config` can construct an `OAuthTargetConfig`.
 */
const OAUTH_PRESETS: Record<
  string,
  {
    token_url: string;
    client_id: string;
    scopes: string[];
  }
> = {
  openai: {
    token_url: "https://auth.openai.com/oauth/token",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    scopes: ["openid", "email", "profile", "offline_access"],
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
  },
  xai: {
    token_url: "https://auth.x.ai/oauth2/token",
    client_id: "b1a00492-073a-47ea-816f-4c329264a828",
    scopes: [
      "openid",
      "profile",
      "email",
      "offline_access",
      "grok-cli:access",
      "api:access",
    ],
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

export default function Providers() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();
  const deleteImpactRequestRef = useRef(0);
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });
  const {
    data: catalog,
    isLoading: catalogLoading,
    isError: catalogError,
  } = useQuery({
    queryKey: ["provider-catalog"],
    queryFn: providerCatalogApi.list,
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
      const parsed = parseCallbackUrl(oauthCallbackUrl, oauthState ?? undefined);
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
    },
    onError: (e: Error) => {
      setOauthError(e.message);
      setOauthMessage(null);
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
    const body: ProviderInput = {
      name: form.name,
      vendor: form.vendor,
      api_base: form.api_base,
      models_endpoint: form.models_endpoint,
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
                    <Td
                      className="truncate font-mono text-xs"
                      title={p.api_base}
                    >
                      {p.api_base}
                    </Td>
                    <Td className="whitespace-nowrap text-xs">
                      {t(authModeLabelKey(p.auth_mode))}
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
          <Field label={t("providers.apiBase")}>
            <Input
              value={form.api_base}
              onChange={(e) => setForm({ ...form, api_base: e.target.value })}
              placeholder={
                catalog?.find((e) => e.id === form.vendor)?.default_base_url ?? ""
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
                    catalog?.find((el) => el.id === form.vendor)?.default_base_url ||
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
                  {hasOAuthMeta(editing) ? (
                    <Badge tone="success">
                      {t("providers.oauthPanel.connected")}
                    </Badge>
                  ) : (
                    <Badge tone="neutral">
                      {t("providers.oauthPanel.notConnected")}
                    </Badge>
                  )}
                </div>
                <div className="flex flex-wrap gap-2">
                  <Button
                    variant="primary"
                    icon={<Play size={16} />}
                    loading={oauthStartMutation.isPending}
                    onClick={() => oauthStartMutation.mutate()}
                  >
                    {t("providers.oauthPanel.start")}
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
              <Alert tone="info">
                {t("providers.oauthPanel.saveFirst")}
              </Alert>
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
