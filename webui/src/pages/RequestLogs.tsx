import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Eye, Copy } from "lucide-react";
import { requestsApi, type RequestFilter } from "@/api/resources";
import type { RequestLogEntry, RequestReplay } from "@/api/types";
import {
  Alert,
  Badge,
  Button,
  Card,
  CardBody,
  Drawer,
  EmptyState,
  ErrorBox,
  Input,
  Spinner,
  Table,
  TableSkeleton,
  Td,
  Th,
  Tooltip,
  Tr,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";

const PAGE_SIZE = 50;

function StatusBadge({
  status,
  errorClass,
}: {
  status: string;
  errorClass?: string | null;
}) {
  if (status === "ok") return <Badge tone="success">{status}</Badge>;
  return (
    <Badge tone="danger" title={status}>
      {errorClass ?? status}
    </Badge>
  );
}

export default function RequestLogs() {
  const { t } = useTranslation();
  const toast = useToast();
  const [filter, setFilter] = useState<RequestFilter>({
    limit: PAGE_SIZE,
    offset: 0,
  });
  const [draft, setDraft] = useState<RequestFilter>({});
  const [detail, setDetail] = useState<RequestLogEntry | null>(null);

  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["requests", filter],
    queryFn: () => requestsApi.list(filter),
  });

  const replayQuery = useQuery<RequestReplay>({
    queryKey: ["replay", detail?.request_id],
    queryFn: () => requestsApi.replay(detail!.request_id),
    enabled: detail !== null,
  });

  const total = data?.total ?? 0;
  const offset = filter.offset ?? 0;

  function applyFilters() {
    setFilter({ ...draft, limit: PAGE_SIZE, offset: 0 });
  }
  function clearFilters() {
    setDraft({});
    setFilter({ limit: PAGE_SIZE, offset: 0 });
  }

  async function copyRequestId() {
    if (!detail) return;
    try {
      await navigator.clipboard.writeText(detail.request_id);
      toast.success(t("requests.idCopied"));
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }

  const entries = data?.entries ?? [];

  return (
    <div className="space-y-4">
      <PageHeader title={t("requests.title")} />

      <Card>
        <CardBody>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Input
              placeholder={t("requests.model")}
              value={draft.model ?? ""}
              onChange={(e) => setDraft({ ...draft, model: e.target.value })}
            />
            <Input
              placeholder={t("requests.provider")}
              value={draft.provider ?? ""}
              onChange={(e) => setDraft({ ...draft, provider: e.target.value })}
            />
            <Input
              placeholder={t("requests.status")}
              value={draft.status ?? ""}
              onChange={(e) => setDraft({ ...draft, status: e.target.value })}
            />
            <Input
              placeholder={t("requests.errorClass")}
              value={draft.error_class ?? ""}
              onChange={(e) =>
                setDraft({ ...draft, error_class: e.target.value })
              }
            />
          </div>
          <div className="mt-3 flex gap-2">
            <Button variant="primary" onClick={applyFilters}>
              {t("requests.apply")}
            </Button>
            <Button variant="secondary" onClick={clearFilters}>
              {t("requests.clear")}
            </Button>
          </div>
        </CardBody>
      </Card>

      {error ? (
        <ErrorBox
          message={(error as Error).message}
          onRetry={() => refetch()}
          retryLabel={t("common.retry")}
        />
      ) : (
        <Card>
          {isLoading ? (
            <TableSkeleton rows={8} />
          ) : entries.length === 0 ? (
            <EmptyState
              title={t("common.emptyTitle")}
              description={t("requests.empty")}
            />
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>{t("requests.ts")}</Th>
                  <Th>{t("requests.model")}</Th>
                  <Th>{t("requests.provider")}</Th>
                  <Th>{t("requests.status")}</Th>
                  <Th className="text-right">{t("requests.latency")}</Th>
                  <Th className="text-right">{t("requests.tokens")}</Th>
                  <Th>{t("requests.cacheHit")}</Th>
                  <Th className="text-right">{t("requests.detail")}</Th>
                </tr>
              </thead>
              <tbody>
                {entries.map((r) => (
                  <Tr key={r.request_id}>
                    <Td className="text-xs text-text-muted">{fmtTime(r.ts)}</Td>
                    <Td>{r.virtual_model || "—"}</Td>
                    <Td>{r.resolved_provider ?? "—"}</Td>
                    <Td>
                      <StatusBadge status={r.status} errorClass={r.error_class} />
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.total_latency_ms}
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.total_tokens ?? "—"}
                    </Td>
                    <Td>{r.cache_hit ?? "—"}</Td>
                    <Td className="text-right">
                      <Tooltip content={t("requests.viewDetail")}>
                        <Button
                          variant="ghost"
                          size="sm"
                          aria-label={t("requests.viewDetail")}
                          onClick={() => setDetail(r)}
                        >
                          <Eye size={14} />
                        </Button>
                      </Tooltip>
                    </Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          )}
          <div className="flex items-center justify-between border-t border-border px-4 py-3 text-sm text-text-muted">
            <span className="tabular-nums">
              {t("requests.total", { count: total })}
            </span>
            <div className="flex gap-2">
              <Button
                variant="secondary"
                size="sm"
                disabled={offset <= 0}
                onClick={() =>
                  setFilter((f) => ({
                    ...f,
                    offset: Math.max(0, (f.offset ?? 0) - PAGE_SIZE),
                  }))
                }
              >
                {t("requests.prev")}
              </Button>
              <Button
                variant="secondary"
                size="sm"
                disabled={offset + PAGE_SIZE >= total}
                onClick={() =>
                  setFilter((f) => ({
                    ...f,
                    offset: (f.offset ?? 0) + PAGE_SIZE,
                  }))
                }
              >
                {t("requests.next")}
              </Button>
            </div>
          </div>
        </Card>
      )}

      <Drawer
        open={detail !== null}
        onOpenChange={(o) => !o && setDetail(null)}
        title={detail?.request_id ?? t("requests.detail")}
        closeLabel={t("common.close")}
        footer={
          <>
            <Button
              variant="secondary"
              icon={<Copy size={14} />}
              onClick={copyRequestId}
            >
              {t("requests.copyId")}
            </Button>
            <Button variant="primary" onClick={() => setDetail(null)}>
              {t("common.close")}
            </Button>
          </>
        }
      >
        <div className="space-y-4 text-sm">
          <div className="grid grid-cols-2 gap-3">
            <Detail label={t("requests.model")} value={detail?.virtual_model} />
            <Detail
              label={t("requests.provider")}
              value={detail?.resolved_provider ?? "—"}
            />
            <Detail label={t("requests.status")} value={detail?.status} />
            <Detail
              label={t("requests.errorClass")}
              value={detail?.error_class ?? "—"}
            />
            <Detail
              label={t("requests.latency")}
              value={String(detail?.total_latency_ms ?? "—")}
            />
            <Detail
              label={t("requests.ttfb")}
              value={String(detail?.ttfb_ms ?? "—")}
            />
          </div>

          <Alert tone="warning">{t("requests.redactedNote")}</Alert>
          <Alert tone="info">{t("requests.snapshotNote")}</Alert>

          {replayQuery.isLoading ? (
            <Spinner />
          ) : replayQuery.error ? (
            <ErrorBox
              message={(replayQuery.error as Error).message}
              onRetry={() => replayQuery.refetch()}
              retryLabel={t("common.retry")}
            />
          ) : (
            <>
              {replayQuery.data?.redacted_headers_json ? (
                <div>
                  <div className="mb-1 text-xs font-medium text-text-muted">
                    {t("requests.redactedHeaders")}
                  </div>
                  <pre className="max-h-40 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
                    {replayQuery.data.redacted_headers_json}
                  </pre>
                </div>
              ) : null}
              {replayQuery.data?.raw_envelope_json ? (
                <div>
                  <div className="mb-1 text-xs font-medium text-text-muted">
                    {t("requests.rawEnvelope")}
                  </div>
                  <pre className="max-h-80 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
                    {replayQuery.data.raw_envelope_json}
                  </pre>
                </div>
              ) : null}
            </>
          )}
        </div>
      </Drawer>
    </div>
  );
}

function Detail({ label, value }: { label: string; value?: string | null }) {
  return (
    <div>
      <div className="text-xs text-text-subtle">{label}</div>
      <div className="text-text">{value || "—"}</div>
    </div>
  );
}
