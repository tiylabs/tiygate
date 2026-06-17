import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, SlidersHorizontal, Ban, Trash2, Copy, Check } from "lucide-react";
import { apiKeysApi } from "@/api/resources";
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
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";

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

export default function ApiKeys() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["api-keys"],
    queryFn: apiKeysApi.list,
  });

  const invalidate = () => qc.invalidateQueries({ queryKey: ["api-keys"] });

  // ---- create ----
  const [createOpen, setCreateOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [createError, setCreateError] = useState<string | null>(null);
  const [secret, setSecret] = useState<CreateApiKeyResponse | null>(null);
  const [copied, setCopied] = useState(false);

  const createMutation = useMutation({
    mutationFn: () => apiKeysApi.create({ name: newName }),
    onSuccess: (res) => {
      setCreateOpen(false);
      setNewName("");
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
            <TableSkeleton />
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
            >
              <colgroup>
                <col style={{ width: "20rem" }} />
                <col style={{ width: "30%" }} />
                <col />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "9rem" }} />
                <col style={{ width: "3.5rem" }} />
              </colgroup>
              <Thead>
                <tr>
                  <Th>{t("common.name")}</Th>
                  <Th>{t("apiKeys.keyHash")}</Th>
                  <Th>{t("apiKeys.quota")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.createdAt")}</Th>
                  <Th className="text-right">{t("common.actions")}</Th>
                </tr>
              </Thead>
              <tbody>
                {(data ?? []).map((k) => (
                  <Tr key={k.id}>
                    <Td className="align-middle">
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
                    <Td className="text-right">
                      <RowActions
                        label={t("common.rowActions")}
                        items={[
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
