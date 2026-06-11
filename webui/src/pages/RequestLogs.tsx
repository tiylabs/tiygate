import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Eye } from "lucide-react";
import { requestsApi, type RequestFilter } from "@/api/resources";
import type { RequestLogEntry, RequestReplay } from "@/api/types";
import {
  Badge,
  Button,
  Card,
  ErrorBox,
  Input,
  Modal,
  Spinner,
  Table,
  Td,
  Th,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";

const PAGE_SIZE = 50;

function StatusBadge({ status, errorClass }: { status: string; errorClass?: string | null }) {
  if (status === "ok") return <Badge tone="green">{status}</Badge>;
  return <Badge tone="red">{errorClass ?? status}</Badge>;
}

export default function RequestLogs() {
  const { t } = useTranslation();
  const [filter, setFilter] = useState<RequestFilter>({ limit: PAGE_SIZE, offset: 0 });
  const [draft, setDraft] = useState<RequestFilter>({});
  const [detail, setDetail] = useState<RequestLogEntry | null>(null);

  const { data, isLoading, error } = useQuery({
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

  return (
    <div className="space-y-4">
      <PageHeader title={t("requests.title")} />

      <Card className="p-4">
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
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
            onChange={(e) => setDraft({ ...draft, error_class: e.target.value })}
          />
        </div>
        <div className="mt-3 flex gap-2">
          <Button variant="primary" onClick={applyFilters}>
            {t("requests.apply")}
          </Button>
          <Button onClick={clearFilters}>{t("requests.clear")}</Button>
        </div>
      </Card>

      {error ? <ErrorBox message={(error as Error).message} /> : null}

      <Card>
        {isLoading ? (
          <Spinner />
        ) : (
          <Table>
            <thead>
              <tr>
                <Th>{t("requests.ts")}</Th>
                <Th>{t("requests.model")}</Th>
                <Th>{t("requests.provider")}</Th>
                <Th>{t("requests.status")}</Th>
                <Th>{t("requests.latency")}</Th>
                <Th>{t("requests.tokens")}</Th>
                <Th>{t("requests.cacheHit")}</Th>
                <Th className="text-right">{t("requests.detail")}</Th>
              </tr>
            </thead>
            <tbody>
              {(data?.entries ?? []).map((r) => (
                <tr key={r.request_id}>
                  <Td className="text-xs text-slate-500">{fmtTime(r.ts)}</Td>
                  <Td>{r.virtual_model || "—"}</Td>
                  <Td>{r.resolved_provider ?? "—"}</Td>
                  <Td>
                    <StatusBadge status={r.status} errorClass={r.error_class} />
                  </Td>
                  <Td>{r.total_latency_ms}</Td>
                  <Td>{r.total_tokens ?? "—"}</Td>
                  <Td>{r.cache_hit ?? "—"}</Td>
                  <Td className="text-right">
                    <Button variant="ghost" onClick={() => setDetail(r)}>
                      <Eye size={14} />
                    </Button>
                  </Td>
                </tr>
              ))}
              {(data?.entries ?? []).length === 0 && !isLoading ? (
                <tr>
                  <Td className="text-slate-400">{t("common.empty")}</Td>
                </tr>
              ) : null}
            </tbody>
          </Table>
        )}
        <div className="flex items-center justify-between border-t border-slate-100 px-4 py-3 text-sm text-slate-500">
          <span>{t("requests.total", { count: total })}</span>
          <div className="flex gap-2">
            <Button
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

      <Modal
        open={detail !== null}
        onClose={() => setDetail(null)}
        title={detail?.request_id ?? t("requests.detail")}
        footer={
          <Button variant="primary" onClick={() => setDetail(null)}>
            {t("common.close")}
          </Button>
        }
      >
        <div className="space-y-3 text-sm">
          <div className="grid grid-cols-2 gap-2">
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

          <p className="text-xs text-amber-600">{t("requests.redactedNote")}</p>

          {replayQuery.isLoading ? (
            <Spinner />
          ) : replayQuery.error ? (
            <ErrorBox message={(replayQuery.error as Error).message} />
          ) : (
            <>
              {replayQuery.data?.redacted_headers_json ? (
                <div>
                  <div className="mb-1 text-xs font-medium text-slate-500">
                    {t("requests.redactedHeaders")}
                  </div>
                  <pre className="max-h-40 overflow-auto rounded-md bg-slate-100 p-3 text-xs">
                    {replayQuery.data.redacted_headers_json}
                  </pre>
                </div>
              ) : null}
              {replayQuery.data?.raw_envelope_json ? (
                <div>
                  <div className="mb-1 text-xs font-medium text-slate-500">
                    {t("requests.rawEnvelope")}
                  </div>
                  <pre className="max-h-60 overflow-auto rounded-md bg-slate-100 p-3 text-xs">
                    {replayQuery.data.raw_envelope_json}
                  </pre>
                </div>
              ) : null}
            </>
          )}
        </div>
      </Modal>
    </div>
  );
}

function Detail({ label, value }: { label: string; value?: string | null }) {
  return (
    <div>
      <div className="text-xs text-slate-400">{label}</div>
      <div className="text-slate-800">{value || "—"}</div>
    </div>
  );
}
