import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { RefreshCw } from "lucide-react";

export function PageHeader({
  title,
  description,
  action,
  onRefresh,
}: {
  title: string;
  description?: ReactNode;
  action?: ReactNode;
  onRefresh?: () => void | Promise<unknown>;
}) {
  const { t } = useTranslation();
  const [isRefreshing, setIsRefreshing] = useState(false);

  async function handleRefresh() {
    if (!onRefresh || isRefreshing) return;
    setIsRefreshing(true);
    try {
      await onRefresh();
    } finally {
      setIsRefreshing(false);
    }
  }

  const refreshLabel = t("common.refresh");

  return (
    <div className="mb-5 flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
      <div className="min-w-0">
        <div className="flex min-w-0 items-center gap-2">
          <h1 className="min-w-0 text-title-lg text-text">{title}</h1>
          {onRefresh ? (
            <button
              type="button"
              aria-label={refreshLabel}
              aria-busy={isRefreshing || undefined}
              title={refreshLabel}
              disabled={isRefreshing}
              onClick={() => void handleRefresh()}
              className="inline-flex h-9 w-9 shrink-0 cursor-pointer items-center justify-center rounded-md border border-border bg-surface text-text-muted transition-[background-color,border-color,color,transform] duration-[var(--duration-fast)] hover:border-border-strong hover:bg-surface-muted hover:text-text active:scale-[0.96] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-bg disabled:cursor-not-allowed disabled:opacity-50"
            >
              <RefreshCw
                size={17}
                aria-hidden
                className={isRefreshing ? "animate-spin" : undefined}
              />
            </button>
          ) : null}
        </div>
        {description ? (
          <p className="mt-1 text-sm text-text-muted">{description}</p>
        ) : null}
      </div>
      {action ? <div className="shrink-0">{action}</div> : null}
    </div>
  );
}

export function shortId(id: string, len = 8): string {
  return id.length > len ? `${id.slice(0, len)}…` : id;
}

export function fmtTime(ts: string | undefined | null): string {
  if (!ts) return "—";
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleString();
}
