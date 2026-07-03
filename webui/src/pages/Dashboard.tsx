import { useCallback, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { apiKeysApi, healthApi, providersApi, statsApi } from "@/api/resources";
import type { CircuitBreaker, StatBucket, StatsResponse } from "@/api/types";
import {
  Badge,
  Card,
  CardHeader,
  EmptyState,
  ErrorBox,
  Metric,
  Table,
  TableSkeleton,
  Thead,
  Td,
  Th,
  Tr,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";
import { InstanceIndicator } from "@/components/InstanceIndicator";
import { TokenSummaryBar } from "@/components/TokenSummaryBar";
import { TokenHeatmap } from "@/components/TokenHeatmap";
import { fmtTokens, fmtUsdFromMicros } from "@/lib/format";

const numberFmt = new Intl.NumberFormat();

function fmtThroughput(value?: number | null): string {
  if (value == null || !Number.isFinite(value)) return "—";
  return value > 200 ? "200+" : value.toFixed(1);
}

/** Two-line cell: primary name on top, faint id below as fallback / context. */
function BucketCell({
  primary,
  secondary,
}: {
  primary: string;
  secondary?: string;
}) {
  if (!secondary || secondary === primary) {
    return <span className="font-medium text-text">{primary || "—"}</span>;
  }
  return (
    <div className="flex flex-col leading-tight">
      <span className="font-medium text-text">{primary}</span>
      <span
        className="font-mono text-[11px] text-text-subtle"
        title={secondary}
      >
        {secondary}
      </span>
    </div>
  );
}

/**
 * Format seconds into a compact human-readable string.
 * e.g. 45 → "45s", 150 → "2m 30s"
 */
function formatRemainingTime(seconds: number): string {
  if (seconds <= 0) return "0s";
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  if (mins === 0) return `${secs}s`;
  if (secs === 0) return `${mins}m`;
  return `${mins}m ${secs}s`;
}

/**
 * Build an i18n-friendly tooltip for a circuit breaker target.
 */
function breakerTooltip(
  b: CircuitBreaker,
  t: (key: string, opts?: Record<string, unknown>) => string,
): string {
  const remaining =
    b.remaining_seconds != null ? formatRemainingTime(b.remaining_seconds) : "";

  if (b.status_kind === "circuit_broken") {
    const tier = Math.max(1, b.consecutive_failures - b.failure_threshold + 1);
    return t("dashboard.breakerTooltipCircuitBroken", {
      failures: b.consecutive_failures,
      threshold: b.failure_threshold,
      tier,
      remaining,
    });
  }

  if (b.status_kind === "cooling") {
    return t("dashboard.breakerTooltipCooling", {
      reason: b.cooling_reason ?? "—",
      remaining,
    });
  }

  return t("dashboard.healthy");
}

function StatsTableContent({
  query,
  resolveBucket,
  resolveSecondary,
  showPerf = false,
}: {
  query: {
    data?: StatsResponse;
    isLoading: boolean;
    error: unknown;
    refetch: () => void;
  };
  resolveBucket?: (id: string) => string | undefined;
  resolveSecondary?: (bucket: string) => string | undefined;
  showPerf?: boolean;
}) {
  const { t } = useTranslation();
  const buckets: StatBucket[] = query.data?.buckets ?? [];

  if (query.isLoading) return <TableSkeleton rows={4} rowHeight="h-14" />;

  if (query.error) {
    return (
      <div className="p-4">
        <ErrorBox
          message={(query.error as Error).message}
          onRetry={() => query.refetch()}
          retryLabel={t("common.retry")}
        />
      </div>
    );
  }

  if (buckets.length === 0) return <EmptyState title={t("common.empty")} />;

  return (
    <Table>
      <Thead>
        <tr>
          <Th>{t("dashboard.bucket")}</Th>
          <Th className="text-right">{t("dashboard.requests")}</Th>
          <Th className="text-right">{t("dashboard.errors")}</Th>
          <Th className="text-right">{t("dashboard.promptTokens")}</Th>
          <Th className="text-right">{t("dashboard.cacheTokens")}</Th>
          <Th className="text-right">{t("dashboard.completionTokens")}</Th>
          <Th className="text-right">{t("dashboard.tokens")}</Th>
          <Th className="text-right">{t("dashboard.cost")}</Th>
          {showPerf && (
            <>
              <Th className="text-right">{t("dashboard.latency")}</Th>
              <Th className="text-right">{t("dashboard.avgThroughput")}</Th>
            </>
          )}
        </tr>
      </Thead>
      <tbody>
        {buckets.map((b) => (
          <Tr key={b.bucket}>
            <Td>
              <BucketCell
                primary={resolveBucket?.(b.bucket) ?? b.bucket}
                secondary={resolveSecondary?.(b.bucket) ?? b.bucket}
              />
            </Td>
            <Td className="text-right tabular-nums">
              {numberFmt.format(b.count)}
            </Td>
            <Td className="text-right tabular-nums">
              {b.error_count > 0 ? (
                <span className="text-danger">
                  {numberFmt.format(b.error_count)}
                </span>
              ) : (
                numberFmt.format(b.error_count)
              )}
            </Td>
            <Td className="text-right tabular-nums">
              {fmtTokens(b.prompt_tokens)}
            </Td>
            <Td className="text-right tabular-nums">
              {fmtTokens(b.cache_read_tokens + b.cache_write_tokens)}
            </Td>
            <Td className="text-right tabular-nums">
              {fmtTokens(b.completion_tokens)}
            </Td>
            <Td className="text-right tabular-nums">
              {fmtTokens(b.total_tokens)}
            </Td>
            <Td className="text-right tabular-nums">
              {fmtUsdFromMicros(b.cost)}
            </Td>
            {showPerf && (
              <>
                <Td className="text-right tabular-nums">
                  {b.avg_latency_ms != null
                    ? numberFmt.format(Math.round(b.avg_latency_ms))
                    : "—"}
                </Td>
                <Td className="text-right tabular-nums">
                  {fmtThroughput(b.avg_throughput_tps)}
                </Td>
              </>
            )}
          </Tr>
        ))}
      </tbody>
    </Table>
  );
}

export default function Dashboard() {
  const { t } = useTranslation();
  const [statsTab, setStatsTab] = useState<
    "model" | "provider" | "apiKey" | "target"
  >("model");

  const byModel = useQuery({
    queryKey: ["stats", "by-model"],
    queryFn: () => statsApi.byModel(),
  });
  const byProvider = useQuery({
    queryKey: ["stats", "by-provider"],
    queryFn: () => statsApi.byProvider(),
  });
  const byApiKey = useQuery({
    queryKey: ["stats", "by-api-key"],
    queryFn: () => statsApi.byApiKey(),
  });
  const byTarget = useQuery({
    queryKey: ["stats", "by-target"],
    queryFn: () => statsApi.byTarget(),
  });
  const breakers = useQuery({
    queryKey: ["circuit-breakers"],
    queryFn: healthApi.circuitBreakers,
  });
  const tokenActivity = useQuery({
    queryKey: ["stats", "token-activity"],
    queryFn: () => statsApi.tokenActivity(365),
    staleTime: 5 * 60_000,
  });
  const tokenSummary = useQuery({
    queryKey: ["stats", "token-summary"],
    queryFn: () => statsApi.tokenSummary(),
    staleTime: 5 * 60_000,
  });
  // Name directories for nicer labels in the stats tables. We keep these
  // queries on a long stale time so they don't refetch with every stats
  // refresh; resource names rarely change.
  const providers = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
    staleTime: 5 * 60_000,
  });
  const apiKeys = useQuery({
    queryKey: ["api-keys"],
    queryFn: apiKeysApi.list,
    staleTime: 5 * 60_000,
  });

  const providerNameById = useMemo(() => {
    const m = new Map<string, string>();
    (providers.data ?? []).forEach((p) => m.set(p.id, p.name));
    return m;
  }, [providers.data]);
  const apiKeyNameById = useMemo(() => {
    const m = new Map<string, string>();
    (apiKeys.data ?? []).forEach((k) => m.set(k.id, k.name));
    return m;
  }, [apiKeys.data]);

  const resolveProviderBucket = useCallback(
    (id: string) => providerNameById.get(id),
    [providerNameById],
  );
  const resolveApiKeyBucket = useCallback(
    (id: string) => apiKeyNameById.get(id),
    [apiKeyNameById],
  );
  const resolveTargetBucket = useCallback(
    (bucket: string) => {
      const idx = bucket.indexOf(" / ");
      if (idx === -1) return undefined;
      const providerId = bucket.slice(0, idx);
      const model = bucket.slice(idx + 3);
      const providerName = providerNameById.get(providerId);
      if (!providerName) return undefined;
      return `${providerName} / ${model}`;
    },
    [providerNameById],
  );
  const resolveTargetSecondary = useCallback((bucket: string) => {
    const idx = bucket.indexOf(" / ");
    if (idx === -1) return undefined;
    return bucket.slice(0, idx);
  }, []);

  // Aggregate top-line metrics from the by-model buckets.
  const modelBuckets = byModel.data?.buckets ?? [];
  const totalRequests = modelBuckets.reduce((s, b) => s + b.count, 0);
  const totalErrors = modelBuckets.reduce((s, b) => s + b.error_count, 0);
  const totalTokens = modelBuckets.reduce((s, b) => s + b.total_tokens, 0);
  const totalCost = modelBuckets.reduce((s, b) => s + b.cost, 0);
  const errorRate = totalRequests > 0 ? (totalErrors / totalRequests) * 100 : 0;

  const targets = breakers.data?.targets ?? [];
  const unhealthy = targets.filter((b) => !b.healthy).length;

  return (
    <div className="space-y-6">
      <PageHeader title={t("dashboard.title")} action={<InstanceIndicator />} />

      {/* 1. Token Activity Heatmap + Summary cards — side by side */}
      <div className="flex flex-col xl:flex-row gap-4 xl:items-stretch">
        {/* Left: heatmap — fixed intrinsic width */}
        <Card className="shrink-0 xl:w-fit">
          {tokenActivity.error || tokenSummary.error ? (
            <div className="p-4">
              <ErrorBox
                message={
                  ((tokenActivity.error ?? tokenSummary.error) as Error).message
                }
                onRetry={() => {
                  tokenActivity.refetch();
                  tokenSummary.refetch();
                }}
                retryLabel={t("common.retry")}
              />
            </div>
          ) : (
            <div className="p-4">
              <TokenHeatmap
                data={tokenActivity.data?.days ?? []}
                isLoading={tokenActivity.isLoading}
              />
            </div>
          )}
        </Card>

        {/* Non-ultrawide: only lifetime Token/cost stays beside the heatmap. */}
        <div className="min-w-0 xl:flex-1 2xl:hidden">
          <TokenSummaryBar
            data={tokenSummary.data}
            isLoading={tokenSummary.isLoading}
            group="lifetime"
          />
        </div>

        {/* Ultrawide: keep the existing full six-card summary layout. */}
        <div className="hidden flex-1 min-w-0 2xl:block">
          <TokenSummaryBar
            data={tokenSummary.data}
            isLoading={tokenSummary.isLoading}
          />
        </div>
      </div>

      {/* Non-ultrawide: detail summary cards span the full row below token activity. */}
      <TokenSummaryBar
        data={tokenSummary.data}
        isLoading={tokenSummary.isLoading}
        group="details"
        className="2xl:hidden"
      />

      {/* Last 24 hours metrics */}
      <div className="space-y-4">
        <h2 className="text-sm font-medium text-text-subtle">
          {t("dashboard.last24hTitle", "Last 24 hours")}
        </h2>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-5">
          <Metric
            label={t("dashboard.breakerStatus")}
            value={
              breakers.isLoading
                ? "…"
                : unhealthy === 0
                  ? t("dashboard.healthy")
                  : unhealthy
            }
            tone={unhealthy === 0 ? "success" : "danger"}
            caption={
              unhealthy === 0
                ? t("dashboard.breakersOk")
                : t("dashboard.breakersDegraded", { count: unhealthy })
            }
          />
          <Metric
            label={t("dashboard.errorRate")}
            value={byModel.isLoading ? "…" : `${errorRate.toFixed(1)}%`}
            tone={errorRate > 0 ? "danger" : "default"}
            caption={`${numberFmt.format(totalErrors)} ${t("dashboard.errors")}`}
          />
          <Metric
            label={t("dashboard.totalTokens")}
            value={byModel.isLoading ? "…" : fmtTokens(totalTokens)}
            caption={t("dashboard.summaryCaption")}
          />
          <Metric
            label={t("dashboard.totalCost")}
            value={byModel.isLoading ? "…" : fmtUsdFromMicros(totalCost)}
            caption={t("dashboard.summaryCaption")}
          />
          <Metric
            label={t("dashboard.totalRequests")}
            value={byModel.isLoading ? "…" : numberFmt.format(totalRequests)}
            caption={t("dashboard.summaryCaption")}
          />
        </div>
      </div>

      <div className="grid gap-6 2xl:grid-cols-2">
        {/* Stats breakdown with tabs */}
        <Card>
          <div className="flex gap-1 border-b border-border px-4 pt-3">
            {(
              [
                { key: "model" as const, label: t("dashboard.byModel") },
                { key: "provider" as const, label: t("dashboard.byProvider") },
                { key: "target" as const, label: t("dashboard.byTarget") },
                { key: "apiKey" as const, label: t("dashboard.byApiKey") },
              ] as const
            ).map((tab) => (
              <button
                key={tab.key}
                onClick={() => setStatsTab(tab.key)}
                className={`rounded-t px-3 py-2 text-sm transition-colors ${
                  statsTab === tab.key
                    ? "border-b-2 border-primary font-medium text-text"
                    : "text-text-subtle hover:text-text"
                }`}
              >
                {tab.label}
              </button>
            ))}
          </div>
          {statsTab === "model" && <StatsTableContent query={byModel} />}
          {statsTab === "provider" && (
            <StatsTableContent
              query={byProvider}
              resolveBucket={resolveProviderBucket}
            />
          )}
          {statsTab === "apiKey" && (
            <StatsTableContent
              query={byApiKey}
              resolveBucket={resolveApiKeyBucket}
            />
          )}
          {statsTab === "target" && (
            <StatsTableContent
              query={byTarget}
              resolveBucket={resolveTargetBucket}
              resolveSecondary={resolveTargetSecondary}
              showPerf
            />
          )}
        </Card>

        <Card>
          <CardHeader title={t("dashboard.circuitBreakers")} />
          {breakers.isLoading ? (
            <TableSkeleton rows={4} rowHeight="h-16" />
          ) : breakers.error ? (
            <div className="p-4">
              <ErrorBox
                message={(breakers.error as Error).message}
                onRetry={() => breakers.refetch()}
                retryLabel={t("common.retry")}
              />
            </div>
          ) : targets.length === 0 ? (
            <EmptyState
              title={breakers.data?.note ?? t("dashboard.noBreakers")}
            />
          ) : (
            <Table>
              <colgroup>
                <col />
                <col style={{ width: "7rem" }} />
              </colgroup>
              <Thead>
                <tr>
                  <Th>{t("dashboard.bucket")}</Th>
                  <Th className="whitespace-nowrap text-right">
                    {t("common.status")}
                  </Th>
                </tr>
              </Thead>
              <tbody>
                {targets.map((b) => {
                  // Prefer the richer label baked in by the backend
                  // (provider_name + model_id), but fall back gracefully
                  // when the gateway returns the older shape.
                  const primary =
                    b.provider_name && b.model_id !== undefined
                      ? `${b.provider_name} / ${b.model_id || "—"}`
                      : (b.provider_name ?? b.target);
                  return (
                    <Tr key={b.target}>
                      <Td className="align-top">
                        <BucketCell primary={primary} secondary={b.target} />
                      </Td>
                      <Td className="text-right align-middle whitespace-nowrap">
                        {b.healthy ? (
                          <Badge tone="success" title={t("dashboard.healthy")}>
                            {t("dashboard.healthy")}
                          </Badge>
                        ) : b.status_kind === "cooling" ? (
                          <div className="flex flex-col items-end gap-1">
                            <Badge tone="warning" title={breakerTooltip(b, t)}>
                              {t("dashboard.breakerCooling")}
                            </Badge>
                            {b.remaining_seconds != null && (
                              <span className="text-xs text-text-subtle">
                                {t("dashboard.breakerRemainingTime", {
                                  time: formatRemainingTime(
                                    b.remaining_seconds,
                                  ),
                                })}
                              </span>
                            )}
                          </div>
                        ) : (
                          <div className="flex flex-col items-end gap-1">
                            <Badge tone="danger" title={breakerTooltip(b, t)}>
                              {t("dashboard.breakerCircuitBroken")}
                            </Badge>
                            {b.remaining_seconds != null && (
                              <span className="text-xs text-text-subtle">
                                {t("dashboard.breakerRemainingTime", {
                                  time: formatRemainingTime(
                                    b.remaining_seconds,
                                  ),
                                })}
                              </span>
                            )}
                          </div>
                        )}
                      </Td>
                    </Tr>
                  );
                })}
              </tbody>
            </Table>
          )}
        </Card>
      </div>

      {/* Circuit breaker rules explanation */}
      <div className="rounded-lg border border-border bg-surface p-5 text-sm text-text-subtle space-y-3">
        <h3 className="text-sm font-medium text-text">
          {t("dashboard.breakerRulesTitle")}
        </h3>
        <ul className="list-disc list-inside space-y-1.5 leading-relaxed">
          <li>{t("dashboard.breakerRuleKey")}</li>
          <li>{t("dashboard.breakerRuleTrip")}</li>
          <li>{t("dashboard.breakerRuleRecovery")}</li>
          <li>{t("dashboard.breakerRuleHalfOpen")}</li>
          <li>{t("dashboard.breakerRuleCoolingRate")}</li>
          <li>{t("dashboard.breakerRuleCoolingAuth")}</li>
          <li>{t("dashboard.breakerRuleScope")}</li>
        </ul>
      </div>
    </div>
  );
}
