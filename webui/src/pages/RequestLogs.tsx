import { useCallback, useEffect, useMemo, useState, type MouseEvent, type ReactNode } from "react";
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

const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;

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
    limit: DEFAULT_PAGE_SIZE,
    offset: 0,
  });
  const limit = filter.limit ?? DEFAULT_PAGE_SIZE;
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
  const page = Math.floor(offset / limit) + 1;
  const pageCount = total === 0 ? 1 : Math.ceil(total / limit);
  const rangeStart = total === 0 ? 0 : offset + 1;
  const rangeEnd = Math.min(offset + limit, total);

  function applyFilters() {
    setFilter({ ...draft, limit, offset: 0 });
  }
  function clearFilters() {
    setDraft({});
    setFilter({ limit, offset: 0 });
  }

  function setPageSize(n: number) {
    setFilter((f) => ({ ...f, limit: n, offset: 0 }));
  }
  function changePage(next: number) {
    const clamped = Math.max(1, Math.min(pageCount, next));
    setFilter((f) => ({ ...f, offset: (clamped - 1) * limit }));
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
          <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 border-t border-border px-4 py-3 text-sm text-text-muted">
            <label className="flex items-center gap-2">
              <span>{t("requests.pageSizeLabel")}</span>
              <select
                aria-label={t("requests.pageSizeLabel")}
                className="h-9 rounded-md border border-border bg-surface px-2 text-sm text-text outline-none focus:border-accent"
                value={limit}
                onChange={(e) => setPageSize(Number(e.target.value))}
              >
                {PAGE_SIZE_OPTIONS.map((n) => (
                  <option key={n} value={n}>
                    {t("requests.pageSizeOption", { count: n })}
                  </option>
                ))}
              </select>
            </label>
            <span className="tabular-nums">
              {total === 0
                ? t("requests.total", { count: 0 })
                : t("requests.range", {
                    from: rangeStart,
                    to: rangeEnd,
                    total,
                  })}
              <span className="mx-2 text-text-subtle">·</span>
              {t("requests.pageOf", { page, total: pageCount })}
            </span>
            <PageNav
              page={page}
              pageCount={pageCount}
              onChange={changePage}
              labels={{
                first: t("requests.firstPage"),
                prev: t("requests.prevPage"),
                next: t("requests.nextPage"),
                last: t("requests.lastPage"),
                goTo: t("requests.goToPage"),
                go: t("requests.go"),
              }}
            />
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
                      <EnvelopeBlock
                        envelopeJson={
                          replayQuery.data?.raw_envelope_json
                        }
                        headersFallbackJson={
                          replayQuery.data?.redacted_headers_json
                        }
                        copyAllLabel={t("requests.copySuccess")}
                      />
                    ),
                  },
                  {
                    label: t("requests.sectionEgressRequest"),
                    content: (
                      <>
                        <MessageBlock
                          mode="request"
                          method={replayQuery.data?.egress_method ?? undefined}
                          path={replayQuery.data?.egress_path ?? undefined}
                          headersJson={replayQuery.data?.egress_headers_json}
                          body={replayQuery.data?.egress_body}
                          bodyTruncated={
                            replayQuery.data?.egress_body_truncated
                          }
                          truncatedNote={t("requests.truncatedNote")}
                          copyAllLabel={t("requests.copySuccess")}
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
                      <MessageBlock
                        mode="response"
                        status={detail?.http_status ?? null}
                        headersJson={replayQuery.data?.client_resp_headers_json}
                        body={replayQuery.data?.client_resp_body}
                        bodyTruncated={
                          replayQuery.data?.client_resp_body_truncated
                        }
                        truncatedNote={t("requests.truncatedNote")}
                        copyAllLabel={t("requests.copySuccess")}
                        streamNote={
                          replayQuery.data?.is_stream
                            ? t("requests.streamNote")
                            : undefined
                        }
                        sseParsed={
                          replayQuery.data?.is_stream &&
                          replayQuery.data?.client_sse_parsed_json
                            ? {
                                label: t("requests.sseParsed"),
                                value: replayQuery.data.client_sse_parsed_json,
                                infoTooltip: t("requests.streamNote"),
                              }
                            : undefined
                        }
                      />
                    ),
                  },
                  {
                    label: t("requests.sectionUpstreamResponse"),
                    content: (
                      <MessageBlock
                        mode="response"
                        status={replayQuery.data?.upstream_status ?? null}
                        headersJson={
                          replayQuery.data?.upstream_resp_headers_json
                        }
                        body={replayQuery.data?.upstream_resp_body}
                        bodyTruncated={
                          replayQuery.data?.upstream_resp_body_truncated
                        }
                        truncatedNote={t("requests.truncatedNote")}
                        copyAllLabel={t("requests.copySuccess")}
                        streamNote={
                          replayQuery.data?.is_stream
                            ? t("requests.streamNote")
                            : undefined
                        }
                        sseParsed={
                          replayQuery.data?.is_stream &&
                          replayQuery.data?.sse_parsed_json
                            ? {
                                label: t("requests.sseParsed"),
                                value: replayQuery.data.sse_parsed_json,
                                infoTooltip: t("requests.streamNote"),
                              }
                            : undefined
                        }
                      />
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

function PageNav({
  page,
  pageCount,
  onChange,
  labels,
}: {
  page: number;
  pageCount: number;
  onChange: (next: number) => void;
  labels: {
    first: string;
    prev: string;
    next: string;
    last: string;
    goTo: string;
    go: string;
  };
}) {
  // Build a window of page numbers: when pageCount <= 7 we render every
  // page; otherwise we show 1, ellipsis, current±2, ellipsis, last.
  // Edge cases at the ends drop the inner ellipsis.
  const visible = buildVisiblePages(page, pageCount);
  const atFirst = page <= 1;
  const atLast = page >= pageCount;
  const baseBtn =
    "inline-flex h-8 min-w-[2rem] items-center justify-center rounded-md border border-border bg-surface px-2 text-xs text-text transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-surface";
  const activeBtn =
    "inline-flex h-8 min-w-[2rem] items-center justify-center rounded-md border border-accent bg-accent/10 px-2 text-xs font-semibold text-accent";
  return (
    <div className="flex flex-wrap items-center gap-1">
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.first}
        disabled={atFirst}
        onClick={() => onChange(1)}
      >
        «
      </button>
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.prev}
        disabled={atFirst}
        onClick={() => onChange(page - 1)}
      >
        ‹
      </button>
      {visible.map((item, i) =>
        item === "…" ? (
          <span
            key={`gap-${i}`}
            className="inline-flex h-8 min-w-[2rem] items-center justify-center text-xs text-text-subtle select-none"
          >
            …
          </span>
        ) : (
          <button
            key={item}
            type="button"
            className={item === page ? activeBtn : baseBtn}
            aria-current={item === page ? "page" : undefined}
            disabled={item === page}
            onClick={() => onChange(item)}
          >
            {item}
          </button>
        ),
      )}
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.next}
        disabled={atLast}
        onClick={() => onChange(page + 1)}
      >
        ›
      </button>
      <button
        type="button"
        className={baseBtn}
        aria-label={labels.last}
        disabled={atLast}
        onClick={() => onChange(pageCount)}
      >
        »
      </button>
      <GotoPage page={page} pageCount={pageCount} onChange={onChange} labels={labels} />
    </div>
  );
}

function GotoPage({
  page,
  pageCount,
  onChange,
  labels,
}: {
  page: number;
  pageCount: number;
  onChange: (next: number) => void;
  labels: { goTo: string; go: string };
}) {
  const [draft, setDraft] = useState<string>(String(page));
  // Keep the input in sync with the external page (e.g. when filters
  // reset the page back to 1).
  useEffect(() => setDraft(String(page)), [page]);
  function commit() {
    const n = Number(draft);
    if (!Number.isFinite(n)) {
      setDraft(String(page));
      return;
    }
    const clamped = Math.max(1, Math.min(pageCount, Math.trunc(n)));
    setDraft(String(clamped));
    if (clamped !== page) onChange(clamped);
  }
  return (
    <form
      className="ml-2 flex items-center gap-1"
      onSubmit={(e) => {
        e.preventDefault();
        commit();
      }}
    >
      <span className="text-xs text-text-subtle">{labels.goTo}</span>
      <input
        type="number"
        min={1}
        max={pageCount}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        className="h-8 w-14 rounded-md border border-border bg-surface px-2 text-xs text-text outline-none focus:border-accent tabular-nums"
      />
      <button
        type="submit"
        className="inline-flex h-8 items-center rounded-md border border-border bg-surface px-2 text-xs text-text hover:bg-surface-muted"
      >
        {labels.go}
      </button>
    </form>
  );
}

function buildVisiblePages(
  page: number,
  pageCount: number,
): (number | "…")[] {
  if (pageCount <= 7) {
    return Array.from({ length: pageCount }, (_, i) => i + 1);
  }
  const set = new Set<number>([1, pageCount, page]);
  for (let i = page - 2; i <= page + 2; i += 1) {
    if (i >= 1 && i <= pageCount) set.add(i);
  }
  const sorted = Array.from(set).sort((a, b) => a - b);
  const out: (number | "…")[] = [];
  for (let i = 0; i < sorted.length; i += 1) {
    if (i > 0 && sorted[i] - sorted[i - 1] > 1) out.push("…");
    out.push(sorted[i]);
  }
  return out;
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

/**
 * Parse a headers payload string into an array of `[key, value]`
 * pairs. The backend serialises headers as a JSON object (BTreeMap /
 * HashMap → `{ "Accept": "..", ... }`). Falls back to a tolerant
 * `[[k, v], ...]` array form when needed, and returns `null` when
 * the payload is missing or unparseable.
 */
function parseHeaders(
  json?: string | null,
): [string, string][] | null {
  if (!json) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return null;
  }
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    return Object.entries(parsed as Record<string, unknown>).map(
      ([k, v]) => [k, typeof v === "string" ? v : JSON.stringify(v)] as [string, string],
    );
  }
  if (Array.isArray(parsed)) {
    const out: [string, string][] = [];
    for (const item of parsed) {
      if (
        Array.isArray(item) &&
        item.length >= 2 &&
        typeof item[0] === "string"
      ) {
        out.push([item[0], String(item[1])]);
      }
    }
    return out;
  }
  return null;
}

/** Pretty-print a JSON body so it lines up like a real packet body. */
function prettyJson(value: string): string {
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

/** Render headers as a K: V table, one row per header (抓包风格). */
function HeadersBlock({
  headers,
  label,
  emptyHint,
  copyAllLabel,
}: {
  headers: [string, string][] | null;
  label?: string;
  emptyHint?: string;
  copyAllLabel: string;
}) {
  const { t } = useTranslation();
  if (headers === null) {
    return emptyHint ? (
      <p className="text-xs text-text-subtle">{emptyHint}</p>
    ) : null;
  }
  const serialized = headers
    .map(([k, v]) => `${k}: ${v}`)
    .join("\n");
  return (
    <div>
      {label ? (
        <div className="mb-1 flex items-center gap-1 text-xs font-medium text-text-muted">
          <span>{label}</span>
          <CopyButton value={serialized} ariaLabel={copyAllLabel} />
        </div>
      ) : (
        <div className="mb-1 flex justify-end">
          <CopyButton value={serialized} ariaLabel={copyAllLabel} />
        </div>
      )}
      <div className="max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
        {headers.length === 0 ? (
          <span className="text-text-subtle">—</span>
        ) : (
          <table className="w-full border-collapse">
            <tbody>
              {headers.map(([k, v], i) => (
                <tr key={`${k}-${i}`} className="align-top">
                  <td className="select-none pr-3 text-text-subtle">{k}</td>
                  <td className="break-all">
                    <span className="select-none text-text-subtle">: </span>
                    {v}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

/**
 * Render a single request or response message in a packet-capture
 * style layout. The first line is a status line:
 *   - `mode="request"` → `<METHOD> <PATH>` (抓包工具的请求首行)
 *   - `mode="response"` → `<STATUS>` (抓包工具的响应首行)
 * Followed by a K: V header table and a body block, both sharing the
 * same `max-h-64` content area so headers and bodies line up.
 */
function MessageBlock({
  mode,
  method,
  path,
  status,
  headersJson,
  body,
  bodyTruncated,
  truncatedNote,
  infoTooltip,
  copyAllLabel,
  streamNote,
  sseParsed,
}: {
  mode: "request" | "response";
  method?: string;
  path?: string;
  status?: number | string | null;
  headersJson?: string | null;
  body?: string | null;
  bodyTruncated?: boolean;
  truncatedNote?: string;
  infoTooltip?: string;
  copyAllLabel: string;
  streamNote?: string;
  sseParsed?: {
    label: string;
    value: string;
    infoTooltip?: string;
  };
}) {
  const { t } = useTranslation();
  const headers = parseHeaders(headersJson);
  const cleanMethod = method?.trim() || undefined;
  const cleanPath = path?.trim() || undefined;
  const statusLine =
    mode === "request"
      ? cleanMethod || cleanPath
        ? `${cleanMethod ?? "?"} ${cleanPath ?? ""}`.trim()
        : ""
      : status !== undefined && status !== null && status !== ""
        ? String(status)
        : "";
  return (
    <div className="space-y-2">
      {statusLine ? (
        <div className="overflow-hidden truncate rounded-md bg-surface-muted px-3 py-1.5 font-mono text-xs text-text">
          <span className="select-none text-text-subtle">
            {mode === "request" ? "→ " : "← "}
          </span>
          {statusLine}
        </div>
      ) : null}
      <HeadersBlock
        headers={headers}
        label={
          mode === "request"
            ? t("requests.sectionRequestHeaders")
            : t("requests.sectionResponseHeaders")
        }
        copyAllLabel={copyAllLabel}
      />
      {mode === "response" && sseParsed?.value ? (
        <SseParsedBlock
          label={sseParsed.label}
          value={sseParsed.value}
          infoTooltip={sseParsed.infoTooltip}
        />
      ) : null}
      {body ? (
        <details className="group">
          <summary className="flex cursor-pointer list-none items-center gap-1 text-xs font-medium text-text-muted select-none [&::-webkit-details-marker]:hidden">
            <ChevronRight
              size={12}
              className="shrink-0 transition-transform group-open:rotate-90"
            />
            <span>
              {mode === "request"
                ? t("requests.sectionRequestBody")
                : t("requests.sectionResponseBodyRaw")}
            </span>
            {bodyTruncated ? (
              <span className="text-text-subtle">{truncatedNote}</span>
            ) : null}
            {streamNote ? (
              <Tooltip content={streamNote}>
                <button
                  type="button"
                  aria-label={streamNote}
                  className="inline-flex h-4 w-4 items-center justify-center rounded text-text-subtle hover:text-text"
                >
                  <Info size={12} />
                </button>
              </Tooltip>
            ) : null}
            <CopyButton value={prettyJson(body)} ariaLabel={copyAllLabel} />
          </summary>
          <pre className="mt-1 max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
            {prettyJson(body)}
          </pre>
        </details>
      ) : null}
      {infoTooltip && !streamNote ? null : null}
    </div>
  );
}

/**
 * Render the `raw_envelope_json` (client → gateway) snapshot as a
 * packet-capture style block. The envelope is a serialised
 * `RawEnvelope` containing `{ method, path, headers, body, ... }`.
 * When the envelope fails to parse we fall back to the redacted
 * headers object so something useful still renders.
 */
function EnvelopeBlock({
  envelopeJson,
  headersFallbackJson,
  copyAllLabel,
}: {
  envelopeJson?: string | null;
  headersFallbackJson?: string | null;
  copyAllLabel: string;
}) {
  const { t } = useTranslation();
  let method: string | undefined;
  let path: string | undefined;
  let headersJson: string | null | undefined = headersFallbackJson;
  let body: string | null | undefined;
  let envelopeTruncated = false;
  if (envelopeJson) {
    try {
      const parsed = JSON.parse(envelopeJson) as Record<string, unknown>;
      if (typeof parsed.method === "string") method = parsed.method;
      if (typeof parsed.path === "string") path = parsed.path;
      if (parsed.headers && typeof parsed.headers === "object") {
        headersJson = JSON.stringify(parsed.headers);
      }
      if (typeof parsed.body === "string") body = parsed.body;
      if (parsed.truncated === true) envelopeTruncated = true;
    } catch {
      // leave defaults
    }
  }
  return (
    <MessageBlock
      mode="request"
      method={method}
      path={path}
      headersJson={headersJson}
      body={body}
      bodyTruncated={envelopeTruncated}
      truncatedNote={t("requests.truncatedNote")}
      copyAllLabel={copyAllLabel}
    />
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
  // Retained for callers that want a collapsible raw SSE preview.
  // The main detail view now renders the upstream body through
  // `MessageBlock`; this is no longer wired up there but the
  // component is kept exported locally for future use.
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

function CopyButton({
  value,
  ariaLabel,
}: {
  value: string;
  ariaLabel?: string;
}) {
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
      aria-label={ariaLabel ?? t("requests.copySuccess")}
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
