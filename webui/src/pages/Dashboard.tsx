import { useCallback, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import {
  apiKeysApi,
  healthApi,
  providersApi,
  statsApi,
} from "@/api/resources";
import type { StatBucket, StatsResponse } from "@/api/types";
import {
  Badge,
  Card,
  CardHeader,
  EmptyState,
  ErrorBox,
  Metric,
  Table,
  TableSkeleton,
  Td,
  Th,
  Tr,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";

const numberFmt = new Intl.NumberFormat();

/** Two-line cell: primary name on top, faint id below as fallback / context. */
function BucketCell({ primary, secondary }: { primary: string; secondary?: string }) {
  if (!secondary || secondary === primary) {
    return (
      <span className="font-medium text-text">{primary || "—"}</span>
    );
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

function StatsTable({
  title,
  query,
  resolveBucket,
}: {
  title: string;
  query: {
    data?: StatsResponse;
    isLoading: boolean;
    error: unknown;
    refetch: () => void;
  };
  /** Map a raw bucket id to a human-friendly name. When omitted the raw
   *  bucket is shown as-is. */
  resolveBucket?: (id: string) => string | undefined;
}) {
  const { t } = useTranslation();
  const buckets: StatBucket[] = query.data?.buckets ?? [];
  return (
    <Card>
      <CardHeader title={title} />
      {query.isLoading ? (
        <TableSkeleton rows={4} />
      ) : query.error ? (
        <div className="p-4">
          <ErrorBox
            message={(query.error as Error).message}
            onRetry={() => query.refetch()}
            retryLabel={t("common.retry")}
          />
        </div>
      ) : buckets.length === 0 ? (
        <EmptyState title={t("common.empty")} />
      ) : (
        <Table>
          <thead>
            <tr>
              <Th>{t("dashboard.bucket")}</Th>
              <Th className="text-right">{t("dashboard.requests")}</Th>
              <Th className="text-right">{t("dashboard.errors")}</Th>
              <Th className="text-right">{t("dashboard.promptTokens")}</Th>
              <Th className="text-right">{t("dashboard.completionTokens")}</Th>
              <Th className="text-right">{t("dashboard.tokens")}</Th>
            </tr>
          </thead>
          <tbody>
            {buckets.map((b) => (
              <Tr key={b.bucket}>
                <Td>
                  <BucketCell
                    primary={resolveBucket?.(b.bucket) ?? b.bucket}
                    secondary={b.bucket}
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
                  {numberFmt.format(b.prompt_tokens)}
                </Td>
                <Td className="text-right tabular-nums">
                  {numberFmt.format(b.completion_tokens)}
                </Td>
                <Td className="text-right tabular-nums">
                  {numberFmt.format(b.total_tokens)}
                </Td>
              </Tr>
            ))}
          </tbody>
        </Table>
      )}
    </Card>
  );
}

export default function Dashboard() {
  const { t } = useTranslation();

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
  const breakers = useQuery({
    queryKey: ["circuit-breakers"],
    queryFn: healthApi.circuitBreakers,
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

  // Aggregate top-line metrics from the by-model buckets.
  const modelBuckets = byModel.data?.buckets ?? [];
  const totalRequests = modelBuckets.reduce((s, b) => s + b.count, 0);
  const totalErrors = modelBuckets.reduce((s, b) => s + b.error_count, 0);
  const totalTokens = modelBuckets.reduce((s, b) => s + b.total_tokens, 0);
  const errorRate =
    totalRequests > 0 ? (totalErrors / totalRequests) * 100 : 0;

  const targets = breakers.data?.targets ?? [];
  const unhealthy = targets.filter((b) => !b.healthy).length;

  return (
    <div className="space-y-6">
      <PageHeader
        title={t("dashboard.title")}
        description={t("dashboard.costUnavailable")}
      />

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
        <Metric
          label={t("dashboard.totalRequests")}
          value={
            byModel.isLoading ? "…" : numberFmt.format(totalRequests)
          }
          caption={t("dashboard.summaryCaption")}
        />
        <Metric
          label={t("dashboard.errorRate")}
          value={byModel.isLoading ? "…" : `${errorRate.toFixed(1)}%`}
          tone={errorRate > 0 ? "danger" : "default"}
          caption={`${numberFmt.format(totalErrors)} ${t("dashboard.errors")}`}
        />
        <Metric
          label={t("dashboard.totalTokens")}
          value={byModel.isLoading ? "…" : numberFmt.format(totalTokens)}
          caption={t("dashboard.summaryCaption")}
        />
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
      </div>

      <div className="grid gap-6 2xl:grid-cols-2">
        <StatsTable title={t("dashboard.byModel")} query={byModel} />
        <StatsTable
          title={t("dashboard.byProvider")}
          query={byProvider}
          resolveBucket={resolveProviderBucket}
        />
        <StatsTable
          title={t("dashboard.byApiKey")}
          query={byApiKey}
          resolveBucket={resolveApiKeyBucket}
        />

        <Card>
          <CardHeader title={t("dashboard.circuitBreakers")} />
          {breakers.isLoading ? (
            <TableSkeleton rows={4} />
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
                <col style={{ width: "5rem" }} />
              </colgroup>
              <thead>
                <tr>
                  <Th>{t("dashboard.bucket")}</Th>
                  <Th className="whitespace-nowrap text-right">
                    {t("common.status")}
                  </Th>
                </tr>
              </thead>
              <tbody>
                {targets.map((b) => {
                  // Prefer the richer label baked in by the backend
                  // (provider_name + model_id), but fall back gracefully
                  // when the gateway returns the older shape.
                  const primary =
                    b.provider_name && b.model_id !== undefined
                      ? `${b.provider_name} / ${b.model_id || "—"}`
                      : b.provider_name ?? b.target;
                  return (
                    <Tr key={b.target}>
                      <Td className="align-top">
                        <BucketCell primary={primary} secondary={b.target} />
                      </Td>
                      <Td className="text-right align-middle whitespace-nowrap">
                        {b.healthy ? (
                          <Badge tone="success" title={b.status}>
                            {t("dashboard.healthy")}
                          </Badge>
                        ) : (
                          <Badge tone="danger" title={b.status}>
                            {t("dashboard.unhealthy")}
                          </Badge>
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
    </div>
  );
}
