import { useCallback, useMemo, useState, type MouseEvent, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Check, ChevronRight, Copy, Eye, Info } from "lucide-react";
import { providersApi, requestsApi, type RequestFilter } from "@/api/resources";
import type { Provider, RequestLogEntry, RequestReplay } from "@/api/types";
import {
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
  // Provider directory: lets us render the upstream column with provider
  // name (e.g. "OpenAI Production") instead of a raw id. Long stale time
  // since names rarely change.
  const { data: providers } = useQuery<Provider[]>({
    queryKey: ["providers"],
    queryFn: providersApi.list,
    staleTime: 5 * 60_000,
  });
  const providerNameById = useMemo(() => {
    const m = new Map<string, string>();
    (providers ?? []).forEach((p) => m.set(p.id, p.name));
    return m;
  }, [providers]);
  const resolveProvider = useCallback(
    (id?: string | null) => (id ? providerNameById.get(id) : undefined),
    [providerNameById],
  );

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
                  <Th>{t("requests.protocol")}</Th>
                  <Th>{t("requests.upstreamModel")}</Th>
                  <Th>{t("requests.status")}</Th>
                  <Th className="text-right">{t("requests.httpStatus")}</Th>
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
                    <Td className="text-xs text-text-muted">
                      <ProtocolCell
                        ingress={r.ingress_protocol}
                        egress={r.egress_protocol}
                      />
                    </Td>
                    <Td
                      className="text-xs"
                      title={r.resolved_provider ?? undefined}
                    >
                      <UpstreamCell
                        provider={r.resolved_provider}
                        model={r.resolved_model}
                        providerName={resolveProvider(r.resolved_provider)}
                      />
                    </Td>
                    <Td>
                      <StatusBadge status={r.status} errorClass={r.error_class} />
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.http_status ?? "—"}
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
        title={
          detail
            ? `${t("requests.requestIdLabel")} ${detail.request_id}`
            : t("requests.detail")
        }
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
              value={
                detail?.resolved_provider
                  ? (resolveProvider(detail.resolved_provider) ??
                    detail.resolved_provider)
                  : "—"
              }
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

          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-text-subtle">
              {t("requests.sectionTokens")}
            </p>
            <div className="grid grid-cols-3 gap-3">
              <Detail
                label={t("requests.tokenPrompt")}
                value={fmtToken(detail?.prompt_tokens)}
              />
              <Detail
                label={t("requests.tokenCompletion")}
                value={fmtToken(detail?.completion_tokens)}
              />
              <Detail
                label={t("requests.tokenReasoning")}
                value={fmtToken(detail?.reasoning_tokens)}
              />
              <Detail
                label={t("requests.tokenCacheRead")}
                value={fmtToken(detail?.cache_read_tokens)}
              />
              <Detail
                label={t("requests.tokenCacheWrite")}
                value={fmtToken(detail?.cache_write_tokens)}
              />
              <Detail
                label={t("requests.tokenTotal")}
                value={fmtToken(detail?.total_tokens)}
              />
            </div>
          </div>

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
              {/* Request group: Client → Gateway / Gateway → Provider */}
              <PayloadTabGroup
                tabs={[
                  {
                    label: t("requests.sectionClientRequest"),
                    content: (
                      <>
                        <PayloadBlock
                          label={t("requests.redactedHeaders")}
                          value={replayQuery.data?.redacted_headers_json}
                        />
                        <PayloadBlock
                          label={t("requests.rawEnvelope")}
                          value={replayQuery.data?.raw_envelope_json}
                        />
                      </>
                    ),
                  },
                  {
                    label: t("requests.sectionEgressRequest"),
                    content: (
                      <>
                        <PayloadBlock
                          label={t("requests.egressHeaders")}
                          value={replayQuery.data?.egress_headers_json}
                        />
                        <PayloadBlock
                          label={t("requests.egressBody")}
                          value={replayQuery.data?.egress_body}
                          truncated={replayQuery.data?.egress_body_truncated}
                          truncatedNote={t("requests.truncatedNote")}
                        />
                      </>
                    ),
                  },
                ]}
              />

              {/* Response group: Gateway → Client / Provider → Gateway */}
              <PayloadTabGroup
                tabs={[
                  {
                    label: t("requests.sectionClientResponse"),
                    content: (
                      <>
                        <PayloadBlock
                          label={t("requests.clientRespHeaders")}
                          value={replayQuery.data?.client_resp_headers_json}
                        />
                        <PayloadBlock
                          label={t("requests.clientRespBody")}
                          value={replayQuery.data?.client_resp_body}
                          truncated={replayQuery.data?.client_resp_body_truncated}
                          truncatedNote={t("requests.truncatedNote")}
                        />
                      </>
                    ),
                  },
                  {
                    label: t("requests.sectionUpstreamResponse"),
                    content: (
                      <>
                        <PayloadBlock
                          label={t("requests.upstreamRespHeaders")}
                          value={replayQuery.data?.upstream_resp_headers_json}
                        />
                        {replayQuery.data?.is_stream &&
                        replayQuery.data?.sse_parsed_json ? (
                          <>
                            <SseParsedBlock
                              label={t("requests.sseParsed")}
                              value={replayQuery.data?.sse_parsed_json}
                              infoTooltip={t("requests.streamNote")}
                            />
                            <SseRawBlock
                              label={t("requests.sseRaw")}
                              value={replayQuery.data?.upstream_resp_body}
                              truncated={
                                replayQuery.data?.upstream_resp_body_truncated
                              }
                              truncatedNote={t("requests.truncatedNote")}
                            />
                          </>
                        ) : (
                          <PayloadBlock
                            label={t("requests.upstreamRespBody")}
                            value={replayQuery.data?.upstream_resp_body}
                            truncated={
                              replayQuery.data?.upstream_resp_body_truncated
                            }
                            truncatedNote={t("requests.truncatedNote")}
                          />
                        )}
                      </>
                    ),
                  },
                ]}
              />
            </>
          )}

          <p className="border-t border-border pt-3 text-xs text-text-subtle">
            {t("requests.replayNote")}
          </p>
        </div>
      </Drawer>
    </div>
  );
}

function fmtToken(value?: number | null): string {
  return value === null || value === undefined ? "—" : value.toLocaleString();
}

function Detail({ label, value }: { label: string; value?: string | null }) {
  return (
    <div>
      <div className="text-xs text-text-subtle">{label}</div>
      <div className="text-text">{value || "—"}</div>
    </div>
  );
}

function PayloadTabGroup({
  tabs,
}: {
  tabs: { label: string; content: ReactNode }[];
}) {
  const [active, setActive] = useState(0);
  return (
    <div className="space-y-2 rounded-md border border-border p-3">
      <div className="flex gap-1 border-b border-border">
        {tabs.map((tab, i) => (
          <button
            key={tab.label}
            type="button"
            onClick={() => setActive(i)}
            className={
              "px-3 py-1.5 text-xs font-semibold -mb-px border-b-2 transition-colors " +
              (i === active
                ? "border-accent text-text"
                : "border-transparent text-text-subtle hover:text-text")
            }
          >
            {tab.label}
          </button>
        ))}
      </div>
      <div className="space-y-2 pt-1">{tabs[active]?.content}</div>
    </div>
  );
}

function PayloadBlock({
  label,
  value,
  truncated,
  truncatedNote,
  infoTooltip,
}: {
  label: string;
  value?: string | null;
  truncated?: boolean;
  truncatedNote?: string;
  infoTooltip?: string;
}) {
  if (!value) return null;
  return (
    <div>
      <BlockHeader
        label={label}
        truncated={truncated}
        truncatedNote={truncatedNote}
        infoTooltip={infoTooltip}
        value={value}
      />
      <pre className="max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
        {value}
      </pre>
    </div>
  );
}

function SseParsedBlock({
  label,
  value,
  infoTooltip,
}: {
  label: string;
  value?: string | null;
  infoTooltip?: string;
}) {
  if (!value) return null;
  return (
    <div>
      <BlockHeader
        label={label}
        infoTooltip={infoTooltip}
        value={value}
      />
      <pre className="max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
        {value}
      </pre>
    </div>
  );
}

function SseRawBlock({
  label,
  value,
  truncated,
  truncatedNote,
}: {
  label: string;
  value?: string | null;
  truncated?: boolean;
  truncatedNote?: string;
}) {
  if (!value) return null;
  return (
    <details className="group">
      <summary className="flex cursor-pointer list-none items-center gap-1 text-xs font-medium text-text-muted select-none [&::-webkit-details-marker]:hidden">
        <ChevronRight
          size={12}
          className="shrink-0 transition-transform group-open:rotate-90"
        />
        <span>{label}</span>
        {truncated ? (
          <span className="ml-1 text-text-subtle">{truncatedNote}</span>
        ) : null}
        <CopyButton value={value} />
      </summary>
      <pre className="mt-1 max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
        {value}
      </pre>
    </details>
  );
}

function BlockHeader({
  label,
  truncated,
  truncatedNote,
  infoTooltip,
  value,
}: {
  label: string;
  truncated?: boolean;
  truncatedNote?: string;
  infoTooltip?: string;
  value: string;
}) {
  return (
    <div className="mb-1 flex items-center gap-1 text-xs font-medium text-text-muted">
      <span>{label}</span>
      {truncated ? (
        <span className="text-text-subtle">{truncatedNote}</span>
      ) : null}
      {infoTooltip ? (
        <Tooltip content={infoTooltip}>
          <button
            type="button"
            aria-label={infoTooltip}
            className="inline-flex h-4 w-4 items-center justify-center rounded text-text-subtle hover:text-text"
          >
            <Info size={12} />
          </button>
        </Tooltip>
      ) : null}
      <CopyButton value={value} />
    </div>
  );
}

function CopyButton({ value }: { value: string }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [done, setDone] = useState(false);
  async function handle(e: MouseEvent) {
    e.stopPropagation();
    e.preventDefault();
    try {
      await navigator.clipboard.writeText(value);
      toast.success(t("requests.copySuccess"));
    } catch {
      toast.error(t("requests.copyFailed"));
    }
    setDone(true);
    window.setTimeout(() => setDone(false), 1200);
  }
  return (
    <button
      type="button"
      onClick={handle}
      aria-label={t("requests.copySuccess")}
      className="ml-auto inline-flex h-5 w-5 items-center justify-center rounded text-text-subtle hover:bg-surface-muted hover:text-text"
    >
      {done ? <Check size={12} /> : <Copy size={12} />}
    </button>
  );
}

function shortProtocol(p?: string | null): string {
  if (!p) return "—";
  // Three-segment form `suite/name/version` → show `name/version`.
  const parts = p.split("/");
  if (parts.length >= 3) return `${parts[1]}/${parts[2]}`;
  return p;
}

function ProtocolCell({
  ingress,
  egress,
}: {
  ingress?: string | null;
  egress?: string | null;
}) {
  const inP = shortProtocol(ingress);
  const eP = egress ? shortProtocol(egress) : null;
  return (
    <Tooltip content={`${ingress ?? "—"} → ${egress ?? "—"}`}>
      <span className="font-mono">
        {inP}
        {eP && eP !== inP ? ` → ${eP}` : ""}
      </span>
    </Tooltip>
  );
}

function UpstreamCell({
  provider,
  model,
  providerName,
}: {
  provider?: string | null;
  model?: string | null;
  providerName?: string;
}) {
  if (!provider && !model) return <span className="text-text-muted">—</span>;
  // Prefer the resolved provider name; fall back to the raw id when
  // the directory hasn't loaded (or the provider was deleted). The raw
  // id is exposed via the parent cell's `title` for inspection without
  // cluttering the visible cell.
  const displayProvider = providerName ?? provider ?? "—";
  return (
    <div className="flex flex-col leading-tight">
      <span>{displayProvider}</span>
      {model ? (
        <span className="font-mono text-[11px] text-text-muted">{model}</span>
      ) : null}
    </div>
  );
}
