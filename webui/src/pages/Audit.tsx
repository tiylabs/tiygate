import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Eye } from "lucide-react";
import { auditApi } from "@/api/resources";
import type { AuditChange, AuditDetails, AuditEntry } from "@/api/types";
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

/** Render a scalar/object audit value into a compact display string. */
function valueToString(value: unknown): string {
  if (value == null) return "—";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return JSON.stringify(value, null, 2);
}

/**
 * Type guard for the structured audit `details` schema
 * (`{snapshot?, changes?}`). Historical records use a flat ad-hoc
 * object (e.g. `{name}`) and are not recognized here, so the UI falls
 * back to the raw JSON view for them.
 */
function asAuditDetails(details: unknown): AuditDetails | null {
  if (details == null || typeof details !== "object") return null;
  const obj = details as Record<string, unknown>;
  const hasSnapshot =
    "snapshot" in obj && typeof obj.snapshot === "object" && obj.snapshot != null;
  const hasChanges = "changes" in obj && Array.isArray(obj.changes);
  if (!hasSnapshot && !hasChanges) return null;
  return {
    snapshot: hasSnapshot ? (obj.snapshot as Record<string, unknown>) : undefined,
    changes: hasChanges ? (obj.changes as AuditChange[]) : undefined,
  };
}

function AuditDetailsView({ details }: { details: unknown }) {
  const { t } = useTranslation();
  const structured = asAuditDetails(details);

  if (!structured) {
    return (
      <pre className="overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text">
        {detailsToString(details)}
      </pre>
    );
  }

  const { snapshot, changes } = structured;

  return (
    <div className="space-y-4">
      {changes !== undefined && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium text-text">{t("audit.changes")}</h3>
          {changes.length === 0 ? (
            <p className="text-xs text-text-muted">{t("audit.noChanges")}</p>
          ) : (
            <Table>
              <thead>
                <tr>
                  <Th>{t("audit.fieldName")}</Th>
                  <Th>{t("audit.before")}</Th>
                  <Th>{t("audit.after")}</Th>
                </tr>
              </thead>
              <tbody>
                {changes.map((c) => (
                  <Tr key={c.field}>
                    <Td className="font-mono text-xs">{c.field}</Td>
                    <Td className="whitespace-pre-wrap font-mono text-xs text-text-muted">
                      {valueToString(c.before)}
                    </Td>
                    <Td className="whitespace-pre-wrap font-mono text-xs">
                      {valueToString(c.after)}
                    </Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          )}
        </section>
      )}

      {snapshot !== undefined && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium text-text">{t("audit.snapshot")}</h3>
          <Table>
            <thead>
              <tr>
                <Th>{t("audit.fieldName")}</Th>
                <Th>{t("audit.details")}</Th>
              </tr>
            </thead>
            <tbody>
              {Object.entries(snapshot).map(([k, v]) => (
                <Tr key={k}>
                  <Td className="font-mono text-xs">{k}</Td>
                  <Td className="whitespace-pre-wrap font-mono text-xs">
                    {valueToString(v)}
                  </Td>
                </Tr>
              ))}
            </tbody>
          </Table>
        </section>
      )}
    </div>
  );
}

/**
 * Extract a human-friendly display name for an audit target from its
 * structured `details.snapshot` (provider/api_key use `name`, route uses
 * `virtual_model`). Returns `null` for legacy records that carry no
 * recognizable snapshot, so callers fall back to showing the id alone.
 */
function targetName(details: unknown): string | null {
  const structured = asAuditDetails(details);
  const snap = structured?.snapshot;
  if (!snap) return null;
  const name = snap.name ?? snap.virtual_model;
  return typeof name === "string" && name.length > 0 ? name : null;
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
                      {(() => {
                        const name = targetName(e.details);
                        return name ? (
                          <>
                            {e.target_type}/{name}
                            <span className="ml-1 text-text-muted">
                              ({e.target_id})
                            </span>
                          </>
                        ) : (
                          <>
                            {e.target_type}/{e.target_id}
                          </>
                        );
                      })()}
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
        <AuditDetailsView details={detail?.details} />
      </Drawer>
    </div>
  );
}
