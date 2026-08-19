import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Plus,
  SlidersHorizontal,
  Ban,
  Trash2,
  Copy,
  Check,
  ShieldCheck,
  Search,
} from "lucide-react";
import { apiKeysApi, routesApi } from "@/api/resources";
import type { ApiKey, CreateApiKeyResponse, QuotaSpec } from "@/api/types";
import {
  Alert,
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Dialog,
  EmptyState,
  ErrorBox,
  Field,
  Input,
  RowActions,
  Table,
  TableSkeleton,
  Thead,
  Td,
  Th,
  Tr,
  useStickyTableScroll,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { cn } from "@/lib/cn";

const QUOTA_FIELDS: Array<{ key: keyof QuotaSpec; label: string }> = [
  { key: "requests_per_minute", label: "apiKeys.rpm" },
  { key: "requests_per_day", label: "apiKeys.rpd" },
  { key: "tokens_per_minute", label: "apiKeys.tpm" },
  { key: "tokens_per_day", label: "apiKeys.tpd" },
];

function quotaSummary(q: QuotaSpec): string {
  const parts: string[] = [];
  if (q.requests_per_minute) parts.push(`${q.requests_per_minute} rpm`);
  if (q.requests_per_day) parts.push(`${q.requests_per_day} rpd`);
  if (q.tokens_per_minute) parts.push(`${q.tokens_per_minute} tpm`);
  if (q.tokens_per_day) parts.push(`${q.tokens_per_day} tpd`);
  return parts.length ? parts.join(", ") : "∞";
}

type ModelAccessMode = "all" | "selected";

async function listAllVirtualModels(): Promise<string[]> {
  const pageSize = 500;
  let offset = 0;
  const models: string[] = [];
  while (true) {
    const page = await routesApi.list({ limit: pageSize, offset });
    models.push(...page.entries.map((route) => route.virtual_model));
    offset += page.entries.length;
    if (page.entries.length === 0 || offset >= page.total) break;
  }
  return Array.from(new Set(models)).sort((a, b) => a.localeCompare(b));
}

function ModelAccessEditor({
  mode,
  selected,
  models,
  loading,
  search,
  onModeChange,
  onSearchChange,
  onToggle,
}: {
  mode: ModelAccessMode;
  selected: string[];
  models: string[];
  loading: boolean;
  search: string;
  onModeChange: (mode: ModelAccessMode) => void;
  onSearchChange: (search: string) => void;
  onToggle: (model: string) => void;
}) {
  const { t } = useTranslation();
  const filtered = useMemo(() => {
    const needle = search.trim().toLowerCase();
    const candidates = Array.from(new Set([...models, ...selected])).sort((a, b) =>
      a.localeCompare(b),
    );
    return needle
      ? candidates.filter((model) => model.toLowerCase().includes(needle))
      : candidates;
  }, [models, search, selected]);

  return (
    <Field label={t("apiKeys.modelAccess")}>
      <div className="inline-flex w-full rounded-md border border-border bg-surface-muted p-1 sm:w-auto">
        {(["all", "selected"] as const).map((option) => (
          <button
            key={option}
            type="button"
            aria-pressed={mode === option}
            className={cn(
              "min-h-8 flex-1 rounded-sm px-3 text-sm font-medium transition-colors sm:flex-none",
              mode === option
                ? "bg-surface text-text shadow-xs"
                : "text-text-muted hover:text-text",
            )}
            onClick={() => onModeChange(option)}
          >
            {t(`apiKeys.modelAccessMode_${option}`)}
          </button>
        ))}
      </div>
      {mode === "selected" ? (
        <div className="mt-3 overflow-hidden rounded-md border border-border">
          <div className="relative border-b border-border bg-surface px-3 py-2">
            <Search
              size={15}
              className="pointer-events-none absolute left-5 top-1/2 -translate-y-1/2 text-text-subtle"
            />
            <Input
              value={search}
              onChange={(event) => onSearchChange(event.target.value)}
              placeholder={t("apiKeys.searchModels")}
              className="pl-8"
            />
          </div>
          <div className="max-h-56 overflow-y-auto bg-surface p-1.5">
            {loading ? (
              <p className="px-2 py-4 text-center text-xs text-text-subtle">
                {t("common.loading")}
              </p>
            ) : filtered.length === 0 ? (
              <p className="px-2 py-4 text-center text-xs text-text-subtle">
                {t("apiKeys.noModels")}
              </p>
            ) : (
              filtered.map((model) => {
                const checked = selected.includes(model);
                return (
                  <label
                    key={model}
                    className="flex cursor-pointer items-center gap-2 rounded-sm px-2 py-2 text-sm text-text hover:bg-surface-muted"
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => onToggle(model)}
                      className="h-4 w-4 accent-primary"
                    />
                    <span className="min-w-0 break-all font-mono text-xs">
                      {model}
                    </span>
                  </label>
                );
              })
            )}
          </div>
          <div className="border-t border-border bg-surface-muted px-3 py-2 text-xs text-text-muted">
            {t("apiKeys.modelsSelected", { count: selected.length })}
          </div>
        </div>
      ) : null}
    </Field>
  );
}

export default function ApiKeys() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["api-keys"],
    queryFn: apiKeysApi.list,
  });
  const routesQuery = useQuery({
    queryKey: ["routes", "api-key-model-access"],
    queryFn: listAllVirtualModels,
  });
  const availableModels = routesQuery.data ?? [];
  const { scrollRef, scrollState } = useStickyTableScroll([
    isLoading,
    data?.length ?? 0,
  ]);

  const invalidate = () => qc.invalidateQueries({ queryKey: ["api-keys"] });

  // ---- create ----
  const [createOpen, setCreateOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [createAccessMode, setCreateAccessMode] = useState<ModelAccessMode>("all");
  const [createModels, setCreateModels] = useState<string[]>([]);
  const [createModelSearch, setCreateModelSearch] = useState("");
  const [createError, setCreateError] = useState<string | null>(null);
  const [secret, setSecret] = useState<CreateApiKeyResponse | null>(null);
  const [copied, setCopied] = useState(false);

  const createMutation = useMutation({
    mutationFn: () =>
      apiKeysApi.create({
        name: newName,
        allowed_models: createAccessMode === "all" ? null : createModels,
      }),
    onSuccess: (res) => {
      setCreateOpen(false);
      setNewName("");
      setCreateAccessMode("all");
      setCreateModels([]);
      setCreateModelSearch("");
      setSecret(res);
      toast.success(t("apiKeys.created"));
      void invalidate();
    },
    onError: (e: Error) => setCreateError(e.message),
  });

  // ---- quota edit ----
  const [quotaKey, setQuotaKey] = useState<ApiKey | null>(null);
  const [quotaForm, setQuotaForm] = useState<Record<string, string>>({});
  const [quotaError, setQuotaError] = useState<string | null>(null);

  const detailQuery = useQuery({
    queryKey: ["api-key", quotaKey?.id],
    queryFn: () => apiKeysApi.get(quotaKey!.id),
    enabled: quotaKey !== null,
  });

  const quotaMutation = useMutation({
    mutationFn: () => {
      const quota: QuotaSpec = {};
      for (const f of QUOTA_FIELDS) {
        const raw = quotaForm[f.key];
        quota[f.key] = raw && raw.trim() ? Number(raw) : null;
      }
      return apiKeysApi.updateQuota(quotaKey!.id, quota);
    },
    onSuccess: () => {
      setQuotaKey(null);
      toast.success(t("common.saved"));
      void invalidate();
    },
    onError: (e: Error) => setQuotaError(e.message),
  });

  const [pendingDisable, setPendingDisable] = useState<ApiKey | null>(null);
  const [pendingDelete, setPendingDelete] = useState<ApiKey | null>(null);
  const [accessKey, setAccessKey] = useState<ApiKey | null>(null);
  const [accessMode, setAccessMode] = useState<ModelAccessMode>("all");
  const [accessModels, setAccessModels] = useState<string[]>([]);
  const [accessModelSearch, setAccessModelSearch] = useState("");
  const [accessError, setAccessError] = useState<string | null>(null);

  const accessMutation = useMutation({
    mutationFn: () =>
      apiKeysApi.updateModelAccess(
        accessKey!.id,
        accessMode === "all" ? null : accessModels,
      ),
    onSuccess: () => {
      setAccessKey(null);
      toast.success(t("common.saved"));
      void invalidate();
    },
    onError: (e: Error) => setAccessError(e.message),
  });

  const disableMutation = useMutation({
    mutationFn: apiKeysApi.disable,
    onSuccess: () => {
      setPendingDisable(null);
      toast.success(t("apiKeys.disabled"));
      void invalidate();
    },
    onError: (e: Error) => {
      setPendingDisable(null);
      toast.error(t("apiKeys.actionFailed"), e.message);
    },
  });
  const deleteMutation = useMutation({
    mutationFn: apiKeysApi.remove,
    onSuccess: () => {
      setPendingDelete(null);
      toast.success(t("apiKeys.deleted"));
      void invalidate();
    },
    onError: (e: Error) => {
      setPendingDelete(null);
      toast.error(t("apiKeys.actionFailed"), e.message);
    },
  });

  function openQuota(k: ApiKey) {
    setQuotaKey(k);
    setQuotaError(null);
    const init: Record<string, string> = {};
    for (const f of QUOTA_FIELDS) {
      const v = k.quota[f.key];
      init[f.key] = v != null ? String(v) : "";
    }
    setQuotaForm(init);
  }

  function openModelAccess(k: ApiKey) {
    setAccessKey(k);
    setAccessError(null);
    setAccessModelSearch("");
    setAccessMode(k.allowed_models == null ? "all" : "selected");
    setAccessModels(k.allowed_models ?? []);
  }

  function toggleModel(
    model: string,
    selected: string[],
    setSelected: (next: string[]) => void,
  ) {
    setSelected(
      selected.includes(model)
        ? selected.filter((item) => item !== model)
        : [...selected, model].sort((a, b) => a.localeCompare(b)),
    );
  }

  function modelAccessSummary(key: ApiKey): string {
    if (key.allowed_models == null) return t("apiKeys.allModels");
    if (key.allowed_models.length === 0) return t("apiKeys.noModelAccess");
    return t("apiKeys.modelCount", { count: key.allowed_models.length });
  }

  async function copySecret() {
    if (!secret) return;
    try {
      await navigator.clipboard.writeText(secret.secret);
      setCopied(true);
      toast.success(t("apiKeys.secretCopied"));
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }

  return (
    <div>
      <PageHeader
        title={t("apiKeys.title")}
        action={
          <Button
            variant="primary"
            icon={<Plus size={16} />}
            onClick={() => {
              setNewName("");
              setCreateAccessMode("all");
              setCreateModels([]);
              setCreateModelSearch("");
              setCreateError(null);
              setCreateOpen(true);
            }}
          >
            {t("apiKeys.add")}
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
              description={t("apiKeys.empty")}
              action={
                <Button
                  variant="primary"
                  icon={<Plus size={16} />}
                  onClick={() => {
                    setNewName("");
                    setCreateAccessMode("all");
                    setCreateModels([]);
                    setCreateModelSearch("");
                    setCreateError(null);
                    setCreateOpen(true);
                  }}
                >
                  {t("apiKeys.add")}
                </Button>
              }
            />
          ) : (
            <Table
              maxHeight={["max-h-[calc(100vh-9.5rem)]", "lg:max-h-[calc(100vh-5.5rem)]"]}
              tableClassName="min-w-max border-separate border-spacing-0"
              containerRef={scrollRef}
            >
              <colgroup>
                <col style={{ width: "20rem" }} />
                <col style={{ width: "30%" }} />
                <col />
                <col style={{ width: "10rem" }} />
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
                  <Th>{t("apiKeys.keyHash")}</Th>
                  <Th>{t("apiKeys.quota")}</Th>
                  <Th>{t("apiKeys.modelAccess")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.createdAt")}</Th>
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
                {(data ?? []).map((k) => (
                  <Tr key={k.id}>
                    <Td
                      className={cn(
                        "sticky left-0 z-10 w-80 bg-surface align-middle group-hover:bg-surface-muted",
                        scrollState !== "start" &&
                          "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
                      <div
                        className="truncate font-medium text-text"
                        title={k.name}
                      >
                        {k.name}
                      </div>
                      <div
                        className="break-all font-mono text-xs text-text-subtle"
                        title={k.id}
                      >
                        {k.id}
                      </div>
                    </Td>
                    <Td
                      className="truncate font-mono text-xs"
                      title={k.key_hash}
                    >
                      {k.key_hash}
                    </Td>
                    <Td
                      className="truncate text-xs tabular-nums"
                      title={quotaSummary(k.quota)}
                    >
                      {quotaSummary(k.quota)}
                    </Td>
                    <Td
                      className="truncate text-xs"
                      title={k.allowed_models?.join(", ") ?? t("apiKeys.allModels")}
                    >
                      {modelAccessSummary(k)}
                    </Td>
                    <Td className="text-center whitespace-nowrap">
                      {k.status === "active" ? (
                        <Badge tone="success">{t("common.enabled")}</Badge>
                      ) : (
                        <Badge tone="neutral">{t("common.disabled")}</Badge>
                      )}
                    </Td>
                    <Td className="whitespace-nowrap text-xs text-text-muted">
                      {fmtTime(k.created_at)}
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
                            key: "model-access",
                            label: t("apiKeys.editModelAccess"),
                            icon: <ShieldCheck size={14} />,
                            onSelect: () => openModelAccess(k),
                          },
                          {
                            key: "quota",
                            label: t("apiKeys.editQuota"),
                            icon: <SlidersHorizontal size={14} />,
                            onSelect: () => openQuota(k),
                          },
                          {
                            key: "disable",
                            label: t("apiKeys.disable"),
                            icon: <Ban size={14} />,
                            disabled: k.status !== "active",
                            onSelect: () => setPendingDisable(k),
                          },
                          {
                            key: "delete",
                            label: t("common.delete"),
                            icon: <Trash2 size={14} />,
                            destructive: true,
                            onSelect: () => setPendingDelete(k),
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

      {/* create dialog */}
      <Dialog
        open={createOpen}
        onOpenChange={setCreateOpen}
        title={t("apiKeys.createTitle")}
        closeLabel={t("common.close")}
        size="lg"
        footer={
          <>
            <Button variant="secondary" onClick={() => setCreateOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              disabled={!newName.trim()}
              loading={createMutation.isPending}
              onClick={() => createMutation.mutate()}
            >
              {t("common.create")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {createError ? <ErrorBox message={createError} /> : null}
          <Field label={t("common.name")} required>
            <Input
              autoFocus
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
            />
          </Field>
          <ModelAccessEditor
            mode={createAccessMode}
            selected={createModels}
            models={availableModels}
            loading={routesQuery.isLoading}
            search={createModelSearch}
            onModeChange={setCreateAccessMode}
            onSearchChange={setCreateModelSearch}
            onToggle={(model) =>
              toggleModel(model, createModels, setCreateModels)
            }
          />
        </div>
      </Dialog>

      {/* one-time secret dialog */}
      <Dialog
        open={secret !== null}
        onOpenChange={(o) => !o && setSecret(null)}
        title={t("apiKeys.secretTitle")}
        closeLabel={t("common.close")}
        footer={
          <Button variant="primary" onClick={() => setSecret(null)}>
            {t("common.close")}
          </Button>
        }
      >
        <div className="space-y-3">
          <Alert tone="warning">{t("apiKeys.secretWarning")}</Alert>
          <div className="flex items-center gap-2">
            <code className="flex-1 break-all rounded-md bg-surface-muted px-3 py-2 font-mono text-sm text-text">
              {secret?.secret}
            </code>
            <Button variant="accent" icon={copied ? <Check size={14} /> : <Copy size={14} />} onClick={copySecret}>
              {copied ? t("common.copied") : t("common.copy")}
            </Button>
          </div>
        </div>
      </Dialog>

      {/* quota edit dialog */}
      <Dialog
        open={quotaKey !== null}
        onOpenChange={(o) => !o && setQuotaKey(null)}
        title={t("apiKeys.quotaTitle", { name: quotaKey?.name ?? "" })}
        closeLabel={t("common.close")}
        footer={
          <>
            <Button variant="secondary" onClick={() => setQuotaKey(null)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              loading={quotaMutation.isPending}
              onClick={() => quotaMutation.mutate()}
            >
              {t("common.save")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {quotaError ? <ErrorBox message={quotaError} /> : null}
          {QUOTA_FIELDS.map((f) => {
            const usage = detailQuery.data?.usage?.[f.key];
            const limitRaw = quotaForm[f.key];
            const limit = limitRaw ? Number(limitRaw) : undefined;
            const pct =
              usage != null && limit && limit > 0
                ? Math.min(100, Math.round((usage / limit) * 100))
                : null;
            return (
              <div key={f.key} className="space-y-1.5">
                <Field label={t(f.label)} hint={t("apiKeys.unlimited")}>
                  <Input
                    type="number"
                    value={quotaForm[f.key] ?? ""}
                    onChange={(e) =>
                      setQuotaForm({ ...quotaForm, [f.key]: e.target.value })
                    }
                  />
                </Field>
                {usage != null ? (
                  <div className="space-y-1">
                    <div className="text-xs text-text-muted tabular-nums">
                      {t("apiKeys.usage")}: {usage}
                      {limit ? ` / ${limit}` : ""}
                    </div>
                    {pct != null ? (
                      <div className="h-1.5 w-full overflow-hidden rounded-full bg-surface-muted">
                        <div
                          className={
                            pct >= 100 ? "h-full bg-danger" : "h-full bg-primary"
                          }
                          style={{ width: `${pct}%` }}
                        />
                      </div>
                    ) : null}
                  </div>
                ) : null}
              </div>
            );
          })}
          {detailQuery.data &&
          Object.keys(detailQuery.data.usage ?? {}).length === 0 ? (
            <p className="text-xs text-text-subtle">
              {t("apiKeys.usageUnavailable")}
            </p>
          ) : null}
        </div>
      </Dialog>

      <Dialog
        open={accessKey !== null}
        onOpenChange={(open) => !open && setAccessKey(null)}
        title={t("apiKeys.modelAccessTitle", { name: accessKey?.name ?? "" })}
        closeLabel={t("common.close")}
        size="lg"
        footer={
          <>
            <Button variant="secondary" onClick={() => setAccessKey(null)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              loading={accessMutation.isPending}
              onClick={() => accessMutation.mutate()}
            >
              {t("common.save")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {accessError ? <ErrorBox message={accessError} /> : null}
          <ModelAccessEditor
            mode={accessMode}
            selected={accessModels}
            models={availableModels}
            loading={routesQuery.isLoading}
            search={accessModelSearch}
            onModeChange={setAccessMode}
            onSearchChange={setAccessModelSearch}
            onToggle={(model) =>
              toggleModel(model, accessModels, setAccessModels)
            }
          />
        </div>
      </Dialog>

      <ConfirmDialog
        open={pendingDisable !== null}
        onOpenChange={(o) => !o && setPendingDisable(null)}
        title={t("apiKeys.disableTitle")}
        description={t("apiKeys.disableConfirm", {
          name: pendingDisable?.name ?? "",
        })}
        confirmLabel={t("apiKeys.disable")}
        cancelLabel={t("common.cancel")}
        destructive
        loading={disableMutation.isPending}
        onConfirm={() =>
          pendingDisable && disableMutation.mutate(pendingDisable.id)
        }
      />

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(o) => !o && setPendingDelete(null)}
        title={t("apiKeys.deleteTitle")}
        description={t("apiKeys.deleteConfirm", {
          name: pendingDelete?.name ?? "",
        })}
        confirmLabel={t("common.delete")}
        cancelLabel={t("common.cancel")}
        destructive
        loading={deleteMutation.isPending}
        onConfirm={() =>
          pendingDelete && deleteMutation.mutate(pendingDelete.id)
        }
      />
    </div>
  );
}
