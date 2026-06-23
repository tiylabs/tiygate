import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import { Eye } from "lucide-react";
import { auditApi, type AuditFilter } from "@/api/resources";
import type { AuditChange, AuditDetails, AuditEntry } from "@/api/types";
import {
  Badge,
  Button,
  Card,
  Drawer,
  EmptyState,
  ErrorBox,
  Table,
  TableSkeleton,
  Thead,
  Td,
  Th,
  Tooltip,
  Tr,
  useStickyTableScroll,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { Pagination } from "@/components/Pagination";
import { cn } from "@/lib/cn";

const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;

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
              <Thead>
                <tr>
                  <Th>{t("audit.fieldName")}</Th>
                  <Th>{t("audit.before")}</Th>
                  <Th>{t("audit.after")}</Th>
                </tr>
              </Thead>
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
            <Thead>
              <tr>
                <Th>{t("audit.fieldName")}</Th>
                <Th>{t("audit.details")}</Th>
              </tr>
            </Thead>
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
  const [filter, setFilter] = useState<AuditFilter>({
    limit: DEFAULT_PAGE_SIZE,
    offset: 0,
  });
  const limit = filter.limit ?? DEFAULT_PAGE_SIZE;
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["audit", filter],
    queryFn: () => auditApi.list(filter),
  });
  const [detail, setDetail] = useState<AuditEntry | null>(null);

  const total = data?.total ?? 0;
  const offset = filter.offset ?? 0;
  const page = Math.floor(offset / limit) + 1;
  const pageCount = total === 0 ? 1 : Math.ceil(total / limit);
  const entries = data?.entries ?? [];
  const { scrollRef, scrollState } = useStickyTableScroll([
    isLoading,
    entries.length,
  ]);

  function setPageSize(n: number) {
    setFilter((f) => ({ ...f, limit: n, offset: 0 }));
  }
  function changePage(next: number) {
    const clamped = Math.max(1, Math.min(pageCount, next));
    setFilter((f) => ({ ...f, offset: (clamped - 1) * limit }));
  }

  return (
    <div className="space-y-4">
      <PageHeader title={t("audit.title")} />
      {error ? (
        <ErrorBox
          message={(error as Error).message}
          onRetry={() => refetch()}
          retryLabel={t("common.retry")}
        />
      ) : (
        <Card>
          {isLoading ? (
            <TableSkeleton
              rows={20}
              rowHeight="h-10"
              className="min-h-[calc(100vh-14rem)] lg:min-h-[calc(100vh-9rem)]"
            />
          ) : entries.length === 0 ? (
            <EmptyState title={t("audit.empty")} />
          ) : (
            <Table
              maxHeight={["max-h-[calc(100vh-14rem)]", "lg:max-h-[calc(100vh-9rem)]"]}
              tableClassName="min-w-max border-separate border-spacing-0"
              containerRef={scrollRef}
            >
              <Thead>
                <tr>
                  <Th
                    className={cn(
                      "sticky left-0 z-30 w-56 bg-surface-muted",
                      scrollState !== "start" &&
                        "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("audit.ts")}
                  </Th>
                  <Th>{t("audit.actor")}</Th>
                  <Th>{t("audit.action")}</Th>
                  <Th>{t("audit.target")}</Th>
                  <Th
                    className={cn(
                      "sticky right-0 z-30 bg-surface-muted text-right",
                      scrollState !== "end" &&
                        "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("audit.details")}
                  </Th>
                </tr>
              </Thead>
              <tbody>
                {entries.map((e) => (
                  <Tr key={e.id}>
                    <Td
                      className={cn(
                        "sticky left-0 z-10 w-56 bg-surface text-xs text-text-muted group-hover:bg-surface-muted",
                        scrollState !== "start" &&
                          "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
                      {fmtTime(e.ts)}
                    </Td>
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
                    <Td
                      className={cn(
                        "sticky right-0 z-10 bg-surface text-right group-hover:bg-surface-muted",
                        scrollState !== "end" &&
                          "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
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
              pageSizeLabel: t("audit.pageSizeLabel"),
              pageSizeOption: t("audit.pageSizeOption"),
              total: t("audit.total"),
              range: t("audit.range"),
              pageOf: t("audit.pageOf"),
              first: t("audit.firstPage"),
              prev: t("audit.prevPage"),
              next: t("audit.nextPage"),
              last: t("audit.lastPage"),
              goTo: t("audit.goToPage"),
              go: t("audit.go"),
            }}
          />
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
