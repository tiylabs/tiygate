import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Eye } from "lucide-react";
import { auditApi } from "@/api/resources";
import type { AuditEntry } from "@/api/types";
import {
  Alert,
  Badge,
  Button,
  Card,
  Drawer,
  EmptyState,
  ErrorBox,
  Table,
  TableSkeleton,
  Td,
  Th,
  Tooltip,
  Tr,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";

function detailsToString(details: unknown): string {
  if (details == null) return "—";
  if (typeof details === "string") return details;
  return JSON.stringify(details, null, 2);
}

export default function Audit() {
  const { t } = useTranslation();
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["audit"],
    queryFn: () => auditApi.list(100),
  });
  const [detail, setDetail] = useState<AuditEntry | null>(null);

  const entries = data ?? [];

  return (
    <div className="space-y-4">
      <PageHeader title={t("audit.title")} />
      <Alert tone="info">{t("audit.immutableNote")}</Alert>
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
            <EmptyState title={t("audit.empty")} />
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>{t("audit.ts")}</Th>
                  <Th>{t("audit.actor")}</Th>
                  <Th>{t("audit.action")}</Th>
                  <Th>{t("audit.target")}</Th>
                  <Th className="text-right">{t("audit.details")}</Th>
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => (
                  <Tr key={e.id}>
                    <Td className="text-xs text-text-muted">{fmtTime(e.ts)}</Td>
                    <Td>{e.actor}</Td>
                    <Td>
                      <Badge tone="primary">{e.action}</Badge>
                    </Td>
                    <Td className="font-mono text-xs">
                      {e.target_type}/{e.target_id}
                    </Td>
                    <Td className="text-right">
                      <Tooltip content={t("audit.viewDetails")}>
                        <Button
                          variant="ghost"
                          size="sm"
                          aria-label={t("audit.viewDetails")}
                          onClick={() => setDetail(e)}
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
        </Card>
      )}

      <Drawer
        open={detail !== null}
        onOpenChange={(o) => !o && setDetail(null)}
        title={t("audit.detailsTitle")}
        description={detail ? `${detail.actor} · ${detail.action}` : undefined}
        closeLabel={t("common.close")}
        footer={
          <Button variant="primary" onClick={() => setDetail(null)}>
            {t("common.close")}
          </Button>
        }
      >
        <pre className="overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
          {detailsToString(detail?.details)}
        </pre>
      </Drawer>
    </div>
  );
}
