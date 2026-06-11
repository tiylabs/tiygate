import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, SlidersHorizontal, Ban, Trash2, Copy, Check } from "lucide-react";
import { apiKeysApi } from "@/api/resources";
import type { ApiKey, CreateApiKeyResponse, QuotaSpec } from "@/api/types";
import {
  Badge,
  Button,
  Card,
  ErrorBox,
  Field,
  Input,
  Modal,
  Spinner,
  Table,
  Td,
  Th,
} from "@/components/ui";
import { PageHeader, fmtTime, shortId } from "@/components/PageHeader";

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
  const { data, isLoading, error } = useQuery({
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
      void invalidate();
    },
    onError: (e: Error) => setQuotaError(e.message),
  });

  const disableMutation = useMutation({
    mutationFn: apiKeysApi.disable,
    onSuccess: () => void invalidate(),
  });
  const deleteMutation = useMutation({
    mutationFn: apiKeysApi.remove,
    onSuccess: () => void invalidate(),
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
    await navigator.clipboard.writeText(secret.secret);
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  }

  return (
    <div>
      <PageHeader
        title={t("apiKeys.title")}
        action={
          <Button
            variant="primary"
            onClick={() => {
              setNewName("");
              setCreateError(null);
              setCreateOpen(true);
            }}
          >
            <Plus size={16} />
            {t("apiKeys.add")}
          </Button>
        }
      />
      {error ? <ErrorBox message={(error as Error).message} /> : null}
      <Card>
        {isLoading ? (
          <Spinner />
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>{t("common.name")}</Th>
                <Th>{t("apiKeys.keyHash")}</Th>
                <Th>{t("apiKeys.quota")}</Th>
                <Th>{t("common.status")}</Th>
                <Th>{t("common.createdAt")}</Th>
                <Th className="text-right">{t("common.actions")}</Th>
              </tr>
            </thead>
            <tbody>
              {(data ?? []).map((k) => (
                <tr key={k.id}>
                  <Td>
                    <div className="font-medium text-slate-800">{k.name}</div>
                    <div className="text-xs text-slate-400">{shortId(k.id)}</div>
                  </Td>
                  <Td className="font-mono text-xs">{shortId(k.key_hash, 16)}</Td>
                  <Td className="text-xs">{quotaSummary(k.quota)}</Td>
                  <Td>
                    {k.status === "active" ? (
                      <Badge tone="green">{k.status}</Badge>
                    ) : (
                      <Badge tone="slate">{k.status}</Badge>
                    )}
                  </Td>
                  <Td className="text-xs text-slate-500">
                    {fmtTime(k.created_at)}
                  </Td>
                  <Td className="text-right">
                    <div className="flex justify-end gap-1">
                      <Button variant="ghost" onClick={() => openQuota(k)}>
                        <SlidersHorizontal size={14} />
                      </Button>
                      <Button
                        variant="ghost"
                        disabled={k.status !== "active"}
                        onClick={() => {
                          if (
                            window.confirm(
                              t("apiKeys.disableConfirm", { name: k.name }),
                            )
                          ) {
                            disableMutation.mutate(k.id);
                          }
                        }}
                      >
                        <Ban size={14} className="text-amber-600" />
                      </Button>
                      <Button
                        variant="ghost"
                        onClick={() => {
                          if (
                            window.confirm(
                              t("apiKeys.deleteConfirm", { name: k.name }),
                            )
                          ) {
                            deleteMutation.mutate(k.id);
                          }
                        }}
                      >
                        <Trash2 size={14} className="text-red-500" />
                      </Button>
                    </div>
                  </Td>
                </tr>
              ))}
              {(data ?? []).length === 0 && !isLoading ? (
                <tr>
                  <Td className="text-slate-400">{t("common.empty")}</Td>
                </tr>
              ) : null}
            </tbody>
          </Table>
        )}
      </Card>

      {/* create modal */}
      <Modal
        open={createOpen}
        onClose={() => setCreateOpen(false)}
        title={t("apiKeys.createTitle")}
        footer={
          <>
            <Button onClick={() => setCreateOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              disabled={!newName.trim() || createMutation.isPending}
              onClick={() => createMutation.mutate()}
            >
              {t("common.create")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {createError ? <ErrorBox message={createError} /> : null}
          <Field label={t("common.name")}>
            <Input
              autoFocus
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
            />
          </Field>
        </div>
      </Modal>

      {/* one-time secret modal */}
      <Modal
        open={secret !== null}
        onClose={() => setSecret(null)}
        title={t("apiKeys.secretTitle")}
        footer={
          <Button variant="primary" onClick={() => setSecret(null)}>
            {t("common.close")}
          </Button>
        }
      >
        <div className="space-y-3">
          <div className="rounded-md border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-800">
            {t("apiKeys.secretWarning")}
          </div>
          <div className="flex items-center gap-2">
            <code className="flex-1 break-all rounded-md bg-slate-100 px-3 py-2 font-mono text-sm">
              {secret?.secret}
            </code>
            <Button onClick={copySecret}>
              {copied ? <Check size={14} /> : <Copy size={14} />}
              {copied ? t("common.copied") : t("common.copy")}
            </Button>
          </div>
        </div>
      </Modal>

      {/* quota edit modal */}
      <Modal
        open={quotaKey !== null}
        onClose={() => setQuotaKey(null)}
        title={t("apiKeys.quotaTitle", { name: quotaKey?.name ?? "" })}
        footer={
          <>
            <Button onClick={() => setQuotaKey(null)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              disabled={quotaMutation.isPending}
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
              <div key={f.key} className="space-y-1">
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
                    <div className="text-xs text-slate-500">
                      {t("apiKeys.usage")}: {usage}
                      {limit ? ` / ${limit}` : ""}
                    </div>
                    {pct != null ? (
                      <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-200">
                        <div
                          className={
                            pct >= 100 ? "h-full bg-red-500" : "h-full bg-slate-700"
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
            <p className="text-xs text-slate-400">
              {t("apiKeys.usageUnavailable")}
            </p>
          ) : null}
        </div>
      </Modal>
    </div>
  );
}
