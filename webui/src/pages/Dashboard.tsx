import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { healthApi, statsApi } from "@/api/resources";
import type { StatBucket, StatsResponse } from "@/api/types";
import { Badge, Card, CardHeader, ErrorBox, Spinner, Table, Td, Th } from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";

function StatsTable({
  title,
  query,
}: {
  title: string;
  query: { data?: StatsResponse; isLoading: boolean; error: unknown };
}) {
  const { t } = useTranslation();
  const buckets: StatBucket[] = query.data?.buckets ?? [];
  return (
    <Card>
      <CardHeader title={title} />
      {query.isLoading ? (
        <Spinner />
      ) : query.error ? (
        <div className="p-4">
          <ErrorBox message={(query.error as Error).message} />
        </div>
      ) : (
        <Table>
          <thead>
            <tr>
              <Th>{t("dashboard.bucket")}</Th>
              <Th>{t("dashboard.requests")}</Th>
              <Th>{t("dashboard.errors")}</Th>
              <Th>{t("dashboard.tokens")}</Th>
            </tr>
          </thead>
          <tbody>
            {buckets.map((b) => (
              <tr key={b.bucket}>
                <Td className="font-medium text-slate-800">{b.bucket || "—"}</Td>
                <Td>{b.count}</Td>
                <Td>
                  {b.error_count > 0 ? (
                    <span className="text-red-600">{b.error_count}</span>
                  ) : (
                    b.error_count
                  )}
                </Td>
                <Td>{b.total_tokens}</Td>
              </tr>
            ))}
            {buckets.length === 0 ? (
              <tr>
                <Td className="text-slate-400">{t("common.empty")}</Td>
              </tr>
            ) : null}
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

  return (
    <div className="space-y-6">
      <PageHeader title={t("dashboard.title")} />

      <p className="text-xs text-slate-400">{t("dashboard.costUnavailable")}</p>

      <div className="grid gap-6 lg:grid-cols-2">
        <StatsTable title={t("dashboard.byModel")} query={byModel} />
        <StatsTable title={t("dashboard.byProvider")} query={byProvider} />
        <StatsTable title={t("dashboard.byApiKey")} query={byApiKey} />

        <Card>
          <CardHeader title={t("dashboard.circuitBreakers")} />
          {breakers.isLoading ? (
            <Spinner />
          ) : breakers.error ? (
            <div className="p-4">
              <ErrorBox message={(breakers.error as Error).message} />
            </div>
          ) : (breakers.data?.targets ?? []).length === 0 ? (
            <p className="p-4 text-sm text-slate-400">
              {breakers.data?.note ?? t("dashboard.noBreakers")}
            </p>
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>{t("dashboard.bucket")}</Th>
                  <Th>{t("common.status")}</Th>
                </tr>
              </thead>
              <tbody>
                {(breakers.data?.targets ?? []).map((b) => (
                  <tr key={b.target}>
                    <Td className="font-mono text-xs">{b.target}</Td>
                    <Td>
                      {b.healthy ? (
                        <Badge tone="green">{t("dashboard.healthy")}</Badge>
                      ) : (
                        <Badge tone="red">{t("dashboard.unhealthy")}</Badge>
                      )}
                      <span className="ml-2 text-xs text-slate-400">
                        {b.status}
                      </span>
                    </Td>
                  </tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>
      </div>
    </div>
  );
}
