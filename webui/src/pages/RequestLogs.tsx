import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type MouseEvent,
  type ReactNode,
} from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import {
  ArrowLeft,
  ArrowRight,
  Check,
  ChevronRight,
  Copy,
  Eye,
  Info,
  Leaf,
} from "lucide-react";
import {
  apiKeysApi,
  providersApi,
  requestsApi,
  type RequestFilter,
} from "@/api/resources";
import type {
  ApiKey,
  Provider,
  RequestLogEntry,
  RequestReplay,
} from "@/api/types";
import {
  Badge,
  Button,
  Card,
  CardBody,
  Drawer,
  EmptyState,
  ErrorBox,
  Input,
  JsonViewer,
  Spinner,
  Switch,
  Table,
  TableSkeleton,
  Td,
  Thead,
  Th,
  Tooltip,
  Tr,
  useStickyTableScroll,
  useToast,
  type BadgeTone,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { Pagination } from "@/components/Pagination";
import { cn } from "@/lib/cn";
import { fmtTokens } from "@/lib/format";

const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;
const AUTO_REFRESH_INTERVAL_MS = 30_000;
const AUTO_REFRESH_STORAGE_KEY = "tiygate.requests.autoRefresh";

function fmtThroughput(value?: number | null): string {
  if (value == null || !Number.isFinite(value)) return "—";
  return value > 200 ? "200+" : value.toFixed(1);
}

function archiveStatusTone(status: string): BadgeTone {
  if (status === "uploaded") return "success";
  if (status === "failed") return "danger";
  if (status === "uploading") return "warning";
  return "neutral";
}

function archiveStatusTitle(status: string) {
  if (status === "archive_ready") {
    return "Payload is fully captured and waiting for archive upload.";
  }
  if (status === "pending") {
    return "Payload capture is persisted but not yet exposed to the archive worker.";
  }
  return status;
}

function StatusBadge({ status }: { status: string }) {
  const { t } = useTranslation();
  // Normalise legacy status values for backward compatibility.
  const normalized =
    status === "ok" ? "success" : status === "error" ? "failed" : status;
  if (normalized === "success") {
    return <Badge tone="success">{t("requests.statusSuccess")}</Badge>;
  }
  if (normalized === "abnormal") {
    return (
      <Badge tone="warning" title={status}>
        {t("requests.statusAbnormal")}
      </Badge>
    );
  }
  return (
    <Badge tone="danger" title={status}>
      {t("requests.statusFailed")}
    </Badge>
  );
}

/// Map a snake_case error_class value to a localised label.
/// Falls back to the raw value when no mapping exists.
function errorClassLabel(
  t: (key: string) => string,
  errorClass?: string | null,
): string {
  if (!errorClass) return "—";
  const key = `requests.errorClass_${errorClass}`;
  const label = t(key);
  // i18next returns the key itself when no translation is found.
  return label === key ? errorClass : label;
}

type FilterOption = {
  value: string;
  label: string;
};

function toFilterOptions(values: string[]): FilterOption[] {
  return values.map((value) => ({ value, label: value }));
}

function filterOptions(options: FilterOption[], query: string) {
  const q = query.trim().toLowerCase();
  if (!q) return options;
  return options.filter(
    (option) =>
      option.label.toLowerCase().includes(q) ||
      option.value.toLowerCase().includes(q),
  );
}

function displayValue(options: FilterOption[], value: string) {
  return options.find((option) => option.value === value)?.label ?? value;
}

function resolveOptionValue(options: FilterOption[], value: string) {
  const trimmed = value.trim();
  const lower = trimmed.toLowerCase();
  return (
    options.find(
      (option) =>
        option.value.toLowerCase() === lower ||
        option.label.toLowerCase() === lower,
    )?.value ?? trimmed
  );
}

function SearchableFilter({
  label,
  placeholder,
  value,
  options,
  onChange,
}: {
  label: string;
  placeholder: string;
  value: string;
  options: FilterOption[];
  onChange: (value: string) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const inputValue = displayValue(options, value);
  const visibleOptions = useMemo(
    () => filterOptions(options, inputValue),
    [options, inputValue],
  );

  return (
    <div className="relative">
      <Input
        aria-label={label}
        placeholder={placeholder}
        value={inputValue}
        onFocus={() => setOpen(true)}
        onBlur={() => {
          onChange(resolveOptionValue(options, inputValue));
          window.setTimeout(() => setOpen(false), 120);
        }}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
        }}
      />
      {open ? (
        <div className="absolute z-50 mt-1 max-h-56 w-full overflow-auto rounded-md border border-border bg-surface p-1 shadow-md">
          <button
            type="button"
            className="block w-full rounded-sm px-2.5 py-1.5 text-left text-sm text-text-subtle hover:bg-surface-muted"
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => {
              onChange("");
              setOpen(false);
            }}
          >
            {t("requests.allOptions")}
          </button>
          {visibleOptions.length === 0 ? (
            <div className="px-2.5 py-1.5 text-sm text-text-subtle">
              {t("requests.noFilterOptions")}
            </div>
          ) : (
            visibleOptions.map((option) => (
              <button
                key={option.value}
                type="button"
                className="block w-full truncate rounded-sm px-2.5 py-1.5 text-left text-sm text-text hover:bg-surface-muted"
                title={
                  option.value === option.label
                    ? option.label
                    : `${option.label} (${option.value})`
                }
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => {
                  onChange(option.value);
                  setOpen(false);
                }}
              >
                {option.label}
              </button>
            ))
          )}
        </div>
      ) : null}
    </div>
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
  const [autoRefresh, setAutoRefresh] = useState<boolean>(() => {
    // sessionStorage keeps the switch sticky across React re-mounts and
    // soft reloads, but is cleared when the tab/window closes — which
    // matches the requested "in-memory for the current session" lifetime
    // and avoids leaving background polling running unnoticed.
    if (typeof window === "undefined") return false;
    return window.sessionStorage.getItem(AUTO_REFRESH_STORAGE_KEY) === "1";
  });

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (autoRefresh) {
      window.sessionStorage.setItem(AUTO_REFRESH_STORAGE_KEY, "1");
    } else {
      window.sessionStorage.removeItem(AUTO_REFRESH_STORAGE_KEY);
    }
  }, [autoRefresh]);

  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["requests", filter],
    queryFn: () => requestsApi.list(filter),
  });

  // Auto-refresh: re-run the current query on a fixed cadence so the
  // list tracks new traffic without a manual reload. Disabled by
  // default and non-persistent — once the page unmounts or the
  // switch flips off, polling stops cleanly.
  useEffect(() => {
    if (!autoRefresh) return;
    const id = window.setInterval(() => {
      refetch();
    }, AUTO_REFRESH_INTERVAL_MS);
    return () => window.clearInterval(id);
  }, [autoRefresh, refetch]);
  const filterOptionsRange = useMemo(
    () => ({ since: filter.since, until: filter.until }),
    [filter.since, filter.until],
  );
  const { data: filterOptionsData } = useQuery({
    queryKey: ["request-filter-options", filterOptionsRange],
    queryFn: () => requestsApi.filterOptions(filterOptionsRange),
    staleTime: 5 * 60_000,
  });
  const requestFilterOptions = useMemo(
    () => ({
      models: filterOptionsData?.models ?? [],
      providers: filterOptionsData?.providers ?? [],
      statuses: filterOptionsData?.statuses ?? [],
      error_classes: filterOptionsData?.error_classes ?? [],
    }),
    [filterOptionsData],
  );
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
  const modelFilterOptions = useMemo(
    () => toFilterOptions(requestFilterOptions.models),
    [requestFilterOptions.models],
  );
  const providerFilterOptions = useMemo(
    () =>
      requestFilterOptions.providers.map((value) => ({
        value,
        label: providerNameById.get(value) ?? value,
      })),
    [providerNameById, requestFilterOptions.providers],
  );
  const statusFilterOptions = useMemo(
    () =>
      Array.from(
        requestFilterOptions.statuses
          .reduce<Map<string, string>>((acc, value) => {
            // Normalise legacy values for display and dedup.
            const normalized =
              value === "ok" ? "success" : value === "error" ? "failed" : value;
            const labelKey = `requests.status${
              normalized.charAt(0).toUpperCase() + normalized.slice(1)
            }`;
            const label = t(labelKey);
            acc.set(normalized, label === labelKey ? normalized : label);
            return acc;
          }, new Map<string, string>())
          .entries(),
      ).map(([value, label]) => ({ value, label })),
    [requestFilterOptions.statuses, t],
  );
  const errorClassFilterOptions = useMemo(
    () =>
      requestFilterOptions.error_classes.map((value) => ({
        value,
        label: errorClassLabel(t, value),
      })),
    [requestFilterOptions.error_classes, t],
  );
  const resolveProvider = useCallback(
    (id?: string | null) => (id ? providerNameById.get(id) : undefined),
    [providerNameById],
  );

  const { data: apiKeys } = useQuery<ApiKey[]>({
    queryKey: ["api-keys"],
    queryFn: apiKeysApi.list,
    staleTime: 5 * 60_000,
  });
  const apiKeyNameById = useMemo(() => {
    const m = new Map<string, string>();
    (apiKeys ?? []).forEach((k) => m.set(k.id, k.name));
    return m;
  }, [apiKeys]);
  const resolveApiKeyName = useCallback(
    (id?: string | null) => (id ? apiKeyNameById.get(id) : undefined),
    [apiKeyNameById],
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

  function applyFilters() {
    const requestId = draft.request_id?.trim();
    if (requestId) {
      setFilter({ request_id: requestId, limit, offset: 0 });
      return;
    }
    setFilter({
      model: draft.model?.trim() || undefined,
      provider:
        resolveOptionValue(providerFilterOptions, draft.provider ?? "") ||
        undefined,
      status: draft.status?.trim() || undefined,
      error_class: draft.error_class?.trim() || undefined,
      limit,
      offset: 0,
    });
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
  const { scrollRef, scrollState } = useStickyTableScroll([
    isLoading,
    entries.length,
  ]);

  return (
    <div className="space-y-4">
      <PageHeader title={t("requests.title")} />

      <Card>
        <CardBody>
          <div className="flex flex-wrap items-start gap-3">
            <Input
              aria-label={t("requests.requestId")}
              placeholder={t("requests.requestId")}
              value={draft.request_id ?? ""}
              onChange={(e) =>
                setDraft({ ...draft, request_id: e.target.value })
              }
              className="min-w-48 flex-1 basis-48"
            />
            <div className="min-w-48 flex-1 basis-48">
              <SearchableFilter
                label={t("requests.model")}
                placeholder={t("requests.modelFilter")}
                value={draft.model ?? ""}
                options={modelFilterOptions}
                onChange={(model) => setDraft({ ...draft, model })}
              />
            </div>
            <div className="min-w-48 flex-1 basis-48">
              <SearchableFilter
                label={t("requests.provider")}
                placeholder={t("requests.providerFilter")}
                value={draft.provider ?? ""}
                options={providerFilterOptions}
                onChange={(provider) => setDraft({ ...draft, provider })}
              />
            </div>
            <div className="min-w-40 flex-1 basis-40">
              <SearchableFilter
                label={t("requests.status")}
                placeholder={t("requests.statusFilter")}
                value={draft.status ?? ""}
                options={statusFilterOptions}
                onChange={(status) => setDraft({ ...draft, status })}
              />
            </div>
            <div className="min-w-48 flex-1 basis-48">
              <SearchableFilter
                label={t("requests.errorClass")}
                placeholder={t("requests.errorClassFilter")}
                value={draft.error_class ?? ""}
                options={errorClassFilterOptions}
                onChange={(error_class) => setDraft({ ...draft, error_class })}
              />
            </div>
            <div className="flex shrink-0 gap-2">
              <Button variant="primary" onClick={applyFilters}>
                {t("requests.apply")}
              </Button>
              <Button variant="secondary" onClick={clearFilters}>
                {t("requests.clear")}
              </Button>
            </div>
          </div>
          <div className="mt-3 flex items-center gap-2 text-xs text-text-subtle">
            <Switch
              checked={autoRefresh}
              onCheckedChange={setAutoRefresh}
              label={t("requests.autoRefresh")}
              aria-label={t("requests.autoRefresh")}
            />
            <span aria-live="polite">
              {autoRefresh
                ? t("requests.autoRefreshOn")
                : t("requests.autoRefreshOff")}
            </span>
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
            <TableSkeleton rows={8} rowHeight="h-14" />
          ) : entries.length === 0 ? (
            <EmptyState
              title={t("common.emptyTitle")}
              description={t("requests.empty")}
            />
          ) : (
            <Table
              maxHeight={[
                "max-h-[calc(100vh-21rem)]",
                "lg:max-h-[calc(100vh-17rem)]",
              ]}
              tableClassName="min-w-max border-separate border-spacing-0"
              containerRef={scrollRef}
            >
              <Thead>
                <tr>
                  <Th
                    className={cn(
                      "sticky left-0 z-30 w-80 bg-surface-muted",
                      scrollState !== "start" &&
                        "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("requests.ts")}
                  </Th>
                  <Th>{t("requests.status")}</Th>
                  <Th className="text-right">{t("requests.httpStatus")}</Th>
                  <Th>{t("requests.model")}</Th>
                  <Th>{t("requests.protocol")}</Th>
                  <Th>{t("requests.upstreamModel")}</Th>
                  <Th className="text-right">{t("requests.cost")}</Th>
                  <Th className="text-right">{t("requests.tokens")}</Th>
                  <Th>{t("requests.cacheHit")}</Th>
                  <Th className="text-right">{t("requests.ttfb")}</Th>
                  <Th className="text-right">{t("requests.outputRate")}</Th>
                  <Th>{t("requests.finishReason")}</Th>
                  <Th
                    className={cn(
                      "sticky right-0 z-30 bg-surface-muted text-right",
                      scrollState !== "end" &&
                        "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("requests.detail")}
                  </Th>
                </tr>
              </Thead>
              <tbody>
                {entries.map((r) => (
                  <Tr key={r.request_id}>
                    <Td
                      className={cn(
                        "sticky left-0 z-10 w-80 bg-surface group-hover:bg-surface-muted text-xs text-text-muted",
                        scrollState !== "start" &&
                          "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
                      <div>{fmtTime(r.ts)}</div>
                      <div className="mt-0.5 flex items-center gap-1 font-mono text-[11px] text-text-subtle min-w-0">
                        <span className="truncate">{r.request_id}</span>
                        <CopyButton
                          value={r.request_id}
                          ariaLabel={t("requests.copyId")}
                          className="shrink-0"
                        />
                      </div>
                    </Td>
                    <Td>
                      <StatusBadge status={r.status} />
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.http_status ?? "—"}
                    </Td>
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
                    <Td className="text-right tabular-nums">
                      {r.cost != null
                        ? `$${(r.cost / 1_000_000).toFixed(6)}`
                        : "—"}
                    </Td>
                    <Td className="text-right tabular-nums">
                      {fmtTokens(r.total_tokens)}
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.cache_read_tokens ? (
                        <div className="flex flex-col leading-tight">
                          <span>{fmtTokens(r.cache_read_tokens)}</span>
                          {r.total_tokens ? (
                            <span className="flex items-center justify-end gap-0.5 text-[11px] text-text-muted">
                              {(r.cache_read_tokens / r.total_tokens) *
                                100 >
                                95 && (
                                <Leaf
                                  size={10}
                                  className="text-success"
                                />
                              )}
                              {(
                                (r.cache_read_tokens / r.total_tokens) *
                                100
                              ).toFixed(1)}
                              %
                            </span>
                          ) : null}
                        </div>
                      ) : (
                        "—"
                      )}
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.ttfb_ms != null
                        ? `${Math.max(0.01, r.ttfb_ms / 1000).toFixed(2)}s`
                        : "—"}
                    </Td>
                    <Td className="text-right tabular-nums">
                      {r.stream_duration_ms &&
                      r.stream_duration_ms > 0 &&
                      r.completion_tokens
                        ? fmtThroughput(
                            r.completion_tokens / (r.stream_duration_ms / 1000),
                          )
                        : "—"}
                    </Td>
                    <Td className="text-xs text-text-muted">
                      {r.finish_reason || "—"}
                    </Td>
                    <Td
                      className={cn(
                        "sticky right-0 z-10 bg-surface group-hover:bg-surface-muted text-right",
                        scrollState !== "end" &&
                          "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
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
          <Pagination
            page={page}
            pageCount={pageCount}
            total={total}
            limit={limit}
            offset={offset}
            pageSizeOptions={PAGE_SIZE_OPTIONS}
            onPageChange={changePage}
            onPageSizeChange={setPageSize}
            labels={{
              pageSizeLabel: t("requests.pageSizeLabel"),
              pageSizeOption: t("requests.pageSizeOption"),
              total: t("requests.total"),
              range: t("requests.range"),
              pageOf: t("requests.pageOf"),
              first: t("requests.firstPage"),
              prev: t("requests.prevPage"),
              next: t("requests.nextPage"),
              last: t("requests.lastPage"),
              goTo: t("requests.goToPage"),
              go: t("requests.go"),
            }}
          />
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
        <div className="space-y-5 text-sm">
          {/* ── Area 1: Summary Bar ── */}
          <div className="flex flex-wrap items-center gap-2">
            <StatusBadge status={detail?.status ?? ""} />
            {detail?.http_status != null && (
              <span className="rounded bg-surface-muted px-1.5 py-0.5 font-mono text-xs tabular-nums">
                {detail.http_status}
              </span>
            )}
            {replayQuery.data?.payload_archive_status && (
              <Badge
                tone={archiveStatusTone(
                  replayQuery.data.payload_archive_status,
                )}
                title={archiveStatusTitle(
                  replayQuery.data.payload_archive_status,
                )}
              >
                {replayQuery.data.payload_archive_status}
              </Badge>
            )}
            <span className="ml-auto text-xs text-text-muted">
              {fmtTime(detail?.ts)}
            </span>
          </div>

          {/* ── Area 2: Summary Metric Tabs ── */}
          <DetailMetricTabGroup
            tabs={[
              {
                label: t("requests.sectionOverview"),
                content: (
                  <div className="grid grid-cols-2 gap-3">
                    <MetricCell
                      label={t("requests.model")}
                      value={detail?.virtual_model}
                    />
                    {detail?.api_key_id && (
                      <MetricCell
                        label={t("requests.apiKeyId")}
                        value={
                          resolveApiKeyName(detail.api_key_id) ??
                          detail.api_key_id
                        }
                      />
                    )}
                    <MetricCell
                      label={t("requests.provider")}
                      value={
                        detail?.resolved_provider
                          ? (resolveProvider(detail.resolved_provider) ??
                            detail.resolved_provider)
                          : undefined
                      }
                    />
                    {detail?.resolved_model && (
                      <MetricCell
                        label={t("requests.resolvedModel")}
                        value={detail.resolved_model}
                      />
                    )}
                    {(detail?.status === "ok" ||
                      detail?.status === "success") &&
                      (replayQuery.data?.finish_reason ??
                        detail?.finish_reason) && (
                      <MetricCell
                        label={t("requests.finishReason")}
                        value={
                          replayQuery.data?.finish_reason ??
                          detail?.finish_reason
                        }
                      />
                    )}
                    {(replayQuery.data?.truncation_reason ??
                      detail?.truncation_reason) && (
                      <MetricCell
                        label={t("requests.truncationReason")}
                        value={
                          replayQuery.data?.truncation_reason ??
                          detail?.truncation_reason
                        }
                      />
                    )}
                    {(detail?.error_class ||
                      (detail?.status !== "ok" &&
                        detail?.status !== "success")) && (
                      <MetricCell
                        label={t("requests.errorClass")}
                        badge={
                          detail?.error_class ? (
                            <Badge tone="danger">
                              {errorClassLabel(t, detail.error_class)}
                            </Badge>
                          ) : undefined
                        }
                      />
                    )}
                    {detail?.error_source && (
                      <MetricCell
                        label={t("requests.errorSource")}
                        value={detail.error_source}
                      />
                    )}
                    {detail?.client_ip && (
                      <MetricCell
                        label={t("requests.clientIp")}
                        value={detail.client_ip}
                        mono
                      />
                    )}
                    {detail?.user_agent && (
                      <MetricCell
                        label={t("requests.userAgent")}
                        value={detail.user_agent}
                      />
                    )}
                  </div>
                ),
              },
              {
                label: t("requests.sectionPerformance"),
                content: (
                  <div className="grid grid-cols-4 gap-3">
                    <MetricCell
                      label={t("requests.latency")}
                      value={detail?.total_latency_ms}
                      mono
                      unit="ms"
                    />
                    <MetricCell
                      label={t("requests.ttfb")}
                      value={detail?.ttfb_ms}
                      mono
                      unit="ms"
                    />
                    <MetricCell
                      label={t("requests.generationTime")}
                      value={detail?.stream_duration_ms}
                      mono
                      unit="ms"
                    />
                    <MetricCell
                      label={t("requests.outputRate")}
                      value={
                        detail?.stream_duration_ms &&
                        detail.stream_duration_ms > 0 &&
                        detail?.completion_tokens
                          ? fmtThroughput(
                              detail.completion_tokens /
                                (detail.stream_duration_ms / 1000),
                            )
                          : undefined
                      }
                      mono
                      unit="tok/s"
                    />
                  </div>
                ),
              },
              {
                label: t("requests.sectionTokens"),
                content: (
                  <div className="grid grid-cols-3 gap-3">
                    <MetricCell
                      label={t("requests.tokenPrompt")}
                      value={fmtTokens(detail?.prompt_tokens)}
                      mono
                    />
                    <MetricCell
                      label={t("requests.tokenCompletion")}
                      value={fmtTokens(detail?.completion_tokens)}
                      mono
                    />
                    <MetricCell
                      label={t("requests.tokenReasoning")}
                      value={fmtTokens(detail?.reasoning_tokens)}
                      mono
                    />
                    <MetricCell
                      label={t("requests.tokenCacheRead")}
                      value={
                        detail?.cache_read_tokens
                          ? detail.total_tokens
                            ? `${fmtTokens(detail.cache_read_tokens)} (${(
                                (detail.cache_read_tokens /
                                  detail.total_tokens) *
                                100
                              ).toFixed(1)}%)`
                            : fmtTokens(detail.cache_read_tokens)
                          : undefined
                      }
                      mono
                    />
                    <MetricCell
                      label={t("requests.tokenCacheWrite")}
                      value={fmtTokens(detail?.cache_write_tokens)}
                      mono
                    />
                    <MetricCell
                      label={t("requests.tokenTotal")}
                      value={fmtTokens(detail?.total_tokens)}
                      mono
                    />
                  </div>
                ),
              },
              {
                label: t("requests.cost"),
                content: (
                  <div className="grid grid-cols-3 gap-3">
                    <div className="col-span-1 row-span-2 flex items-center">
                      <MetricCell
                        label={t("requests.costTotal")}
                        value={
                          detail?.cost != null
                            ? `$${(detail.cost / 1_000_000).toFixed(6)}`
                            : undefined
                        }
                        mono
                      />
                    </div>
                    <MetricCell
                      label={t("requests.costInput")}
                      value={
                        detail?.input_cost != null
                          ? `$${(detail.input_cost / 1_000_000).toFixed(6)}`
                          : undefined
                      }
                      mono
                    />
                    <MetricCell
                      label={t("requests.costOutput")}
                      value={
                        detail?.output_cost != null
                          ? `$${(detail.output_cost / 1_000_000).toFixed(6)}`
                          : undefined
                      }
                      mono
                    />
                    <MetricCell
                      label={t("requests.costCacheRead")}
                      value={
                        detail?.cache_read_cost != null
                          ? `$${(detail.cache_read_cost / 1_000_000).toFixed(6)}`
                          : undefined
                      }
                      mono
                    />
                    <MetricCell
                      label={t("requests.costCacheWrite")}
                      value={
                        detail?.cache_write_cost != null
                          ? `$${(detail.cache_write_cost / 1_000_000).toFixed(6)}`
                          : undefined
                      }
                      mono
                    />
                  </div>
                ),
              },
            ]}
          />

          {/* ── Area 4: Unified Payload Tabs ── */}
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
              {/* Request: Client → Gateway / Gateway → Provider */}
              <PayloadTabGroup
                title={t("requests.payloadRequest")}
                tabs={[
                  {
                    label: t("requests.tabClientGateway"),
                    content: (
                      <EnvelopeBlock
                        envelopeJson={replayQuery.data?.raw_envelope_json}
                        headersFallbackJson={
                          replayQuery.data?.redacted_headers_json
                        }
                        copyAllLabel={t("requests.copySuccess")}
                      />
                    ),
                  },
                  {
                    label: t("requests.tabGatewayProvider"),
                    content: (
                      <MessageBlock
                        mode="request"
                        method={replayQuery.data?.egress_method ?? undefined}
                        path={replayQuery.data?.egress_path ?? undefined}
                        headersJson={replayQuery.data?.egress_headers_json}
                        body={replayQuery.data?.egress_body}
                        copyAllLabel={t("requests.copySuccess")}
                      />
                    ),
                  },
                ]}
              />

              {/* Response: Gateway → Client / Provider → Gateway */}
              <PayloadTabGroup
                title={t("requests.payloadResponse")}
                tabs={[
                  {
                    label: t("requests.tabGatewayClient"),
                    content: (
                      <MessageBlock
                        mode="response"
                        status={detail?.http_status ?? null}
                        headersJson={replayQuery.data?.client_resp_headers_json}
                        body={replayQuery.data?.client_resp_body}
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
                    label: t("requests.tabProviderGateway"),
                    content: (
                      <MessageBlock
                        mode="response"
                        status={replayQuery.data?.upstream_status ?? null}
                        headersJson={
                          replayQuery.data?.upstream_resp_headers_json
                        }
                        body={replayQuery.data?.upstream_resp_body}
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

          {/* ── Area 5: Replay Note ── */}
          <div className="rounded-md border border-border bg-surface-muted px-3 py-2 text-xs text-text-subtle">
            {t("requests.replayNote")}
          </div>
        </div>
      </Drawer>
    </div>
  );
}

function MetricCell({
  label,
  value,
  mono,
  unit,
  badge,
}: {
  label: string;
  value?: string | number | null;
  mono?: boolean;
  unit?: string;
  badge?: ReactNode;
}) {
  const display =
    value === null || value === undefined || value === "" ? "—" : String(value);
  return (
    <div>
      <div className="text-xs text-text-subtle">{label}</div>
      {badge ? (
        <div className="mt-0.5">{badge}</div>
      ) : (
        <div
          className={cn(
            "text-text break-words",
            mono && "font-mono tabular-nums",
          )}
        >
          {display}
          {unit && display !== "—" ? (
            <span className="ml-0.5 text-text-subtle">{unit}</span>
          ) : null}
        </div>
      )}
    </div>
  );
}

function DetailMetricTabGroup({
  tabs,
}: {
  tabs: { label: string; content: ReactNode }[];
}) {
  const [active, setActive] = useState(0);
  return (
    <div className="space-y-3 rounded-md border border-border bg-surface p-3">
      <div className="flex gap-1 overflow-x-auto border-b border-border" role="tablist">
        {tabs.map((tab, i) => (
          <button
            key={tab.label}
            type="button"
            role="tab"
            aria-selected={i === active}
            onClick={() => setActive(i)}
            className={
              "-mb-px whitespace-nowrap border-b-2 px-3 py-1.5 text-xs font-medium uppercase tracking-wide transition-colors " +
              (i === active
                ? "border-accent text-text"
                : "border-transparent text-text-subtle hover:text-text")
            }
          >
            {tab.label}
          </button>
        ))}
      </div>
      <div role="tabpanel">{tabs[active]?.content}</div>
    </div>
  );
}

function PayloadTabGroup({
  title,
  tabs,
}: {
  title: string;
  tabs: { label: string; content: ReactNode }[];
}) {
  const [active, setActive] = useState(0);
  return (
    <div className="space-y-2 rounded-md border border-border p-3">
      <div className="flex items-center gap-2 border-b border-border">
        <h3 className="shrink-0 text-xs font-medium uppercase tracking-wide text-text-subtle">
          {title}
        </h3>
        <div className="ml-auto flex gap-1" role="tablist">
          {tabs.map((tab, i) => (
            <button
              key={tab.label}
              type="button"
              role="tab"
              aria-selected={i === active}
              onClick={() => setActive(i)}
              className={
                "whitespace-nowrap px-2 py-1.5 text-xs font-medium -mb-px border-b-2 transition-colors " +
                (i === active
                  ? "border-accent text-text"
                  : "border-transparent text-text-subtle hover:text-text")
              }
            >
              {tab.label}
            </button>
          ))}
        </div>
      </div>
      <div className="space-y-2 pt-1" role="tabpanel">
        {tabs[active]?.content}
      </div>
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
function parseHeaders(json?: string | null): [string, string][] | null {
  if (!json) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return null;
  }
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    return Object.entries(parsed as Record<string, unknown>).map(
      ([k, v]) =>
        [k, typeof v === "string" ? v : JSON.stringify(v)] as [string, string],
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

/** Render headers as a K: V table, one row per header (抓包风格).
 *  Wrapped in a `<details>` so the user can collapse the header block
 *  to focus on the body. Headers are noise-heavy by default, so the
 *  block starts collapsed. */
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
  if (headers === null) {
    return emptyHint ? (
      <p className="text-xs text-text-subtle">{emptyHint}</p>
    ) : null;
  }
  const serialized = headers.map(([k, v]) => `${k}: ${v}`).join("\n");
  return (
    <details className="group">
      <summary className="flex cursor-pointer list-none items-center gap-1 text-xs font-medium text-text-muted select-none [&::-webkit-details-marker]:hidden">
        <ChevronRight
          size={12}
          className="shrink-0 transition-transform group-open:rotate-90"
        />
        <span>{label}</span>
        {label ? (
          <CopyButton value={serialized} ariaLabel={copyAllLabel} />
        ) : (
          <CopyButton
            value={serialized}
            ariaLabel={copyAllLabel}
            className="ml-auto"
          />
        )}
      </summary>
      <div className="mt-1 max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
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
    </details>
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
        <details
          className="group"
          // Body default state depends on direction: the *request* body is
          // usually short and worth scanning at a glance, so we keep it
          // open. The *raw* response body (esp. SSE streams) is often
          // huge and noisy, so we leave it collapsed unless the user
          // expands it explicitly.
          open={mode === "request"}
        >
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
            {streamNote && mode === "request" ? (
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
          <JsonViewer value={body} className="mt-1" />
        </details>
      ) : null}
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
  let method: string | undefined;
  let path: string | undefined;
  let headersJson: string | null | undefined = headersFallbackJson;
  let body: string | null | undefined;
  if (envelopeJson) {
    try {
      const parsed = JSON.parse(envelopeJson) as Record<string, unknown>;
      if (typeof parsed.method === "string") method = parsed.method;
      if (typeof parsed.path === "string") path = parsed.path;
      if (parsed.headers && typeof parsed.headers === "object") {
        headersJson = JSON.stringify(parsed.headers);
      }
      if (typeof parsed.body === "string") body = parsed.body;
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
      copyAllLabel={copyAllLabel}
    />
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
    <details open className="group">
      <summary className="flex cursor-pointer list-none items-center gap-1 text-xs font-medium text-text-muted select-none [&::-webkit-details-marker]:hidden">
        <ChevronRight
          size={12}
          className="shrink-0 transition-transform group-open:rotate-90"
        />
        <span>{label}</span>
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
        <CopyButton value={prettyJson(value)} />
      </summary>
      <JsonViewer value={value} className="mt-1" />
    </details>
  );
}

function CopyButton({
  value,
  ariaLabel,
  className,
}: {
  value: string;
  ariaLabel?: string;
  className?: string;
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
      className={cn(
        "inline-flex h-5 w-5 items-center justify-center rounded text-text-subtle hover:bg-surface-muted hover:text-text",
        className ?? "ml-auto",
      )}
    >
      {done ? <Check size={12} /> : <Copy size={12} />}
    </button>
  );
}

function protocolCategory(
  p?: string | null,
): { label: string; tone: BadgeTone } | null {
  if (!p) return null;
  // Three-segment form `suite/name/version` → category derived from the name.
  const parts = p.split("/");
  const name =
    parts.length >= 3 ? parts[1] : parts.length === 2 ? parts[1] : parts[0];
  switch (name) {
    case "chat-completions":
      return { label: "OpenAI-Compatible", tone: "primary" };
    case "responses":
      return { label: "Responses", tone: "success" };
    case "messages":
      return { label: "Messages", tone: "warning" };
    case "generateContent":
      return { label: "Gemini", tone: "info" };
    case "embeddings":
      return { label: "Embedding", tone: "neutral" };
    case "images-generations":
    case "images-edits":
      return { label: "OpenAI-Images", tone: "primary" };
    default:
      return { label: name, tone: "neutral" };
  }
}

function ProtocolCell({
  ingress,
  egress,
}: {
  ingress?: string | null;
  egress?: string | null;
}) {
  const inCat = protocolCategory(ingress);
  const eCat = egress ? protocolCategory(egress) : null;
  const same = eCat && inCat && eCat.label === inCat.label;
  return (
    <span className="inline-flex items-center gap-1">
      {inCat ? (
        <Badge tone={inCat.tone}>{inCat.label}</Badge>
      ) : (
        <span className="text-text-muted">—</span>
      )}
      {eCat && !same ? (
        <>
          <span className="inline-flex flex-col items-center leading-none text-text-muted">
            <ArrowRight size={10} />
            <ArrowLeft size={10} />
          </span>
          <Badge tone={eCat.tone}>{eCat.label}</Badge>
        </>
      ) : null}
    </span>
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
