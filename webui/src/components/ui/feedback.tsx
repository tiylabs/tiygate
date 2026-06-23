import type { ReactNode } from "react";
import { AlertTriangle, Inbox, RotateCw } from "lucide-react";
import { cn } from "@/lib/cn";
import { Button } from "./button";

export function Spinner({ className }: { className?: string }) {
  return (
    <div
      className={cn(
        "flex items-center justify-center py-10 text-text-subtle",
        className,
      )}
      role="status"
      aria-live="polite"
    >
      <div className="h-5 w-5 animate-spin rounded-full border-2 border-border border-t-primary" />
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return (
    <div
      className={cn(
        "animate-pulse rounded-md bg-surface-muted",
        className,
      )}
      aria-hidden
    />
  );
}

/**
 * Table skeleton placeholder (docs §9: >1s list loading prefers skeleton).
 * `rowHeight` should approximate the real row height so the skeleton does not
 * collapse shorter than the loaded content, which causes a visible jump.
 */
export function TableSkeleton({
  rows = 5,
  rowHeight = "h-9",
  className,
}: {
  rows?: number;
  rowHeight?: string;
  className?: string;
}) {
  // When a min-h/flex container is supplied via className, stretch each row
  // with flex-1 so they evenly fill the available height instead of leaving
  // a gap at the bottom.
  const fill = !!className;
  return (
    <div
      className={cn(
        "p-4",
        fill ? "flex flex-col gap-2" : "space-y-2",
        className,
      )}
      aria-hidden
    >
      {Array.from({ length: rows }).map((_, i) => (
        <Skeleton
          key={i}
          className={cn("w-full", fill ? "flex-1" : rowHeight)}
        />
      ))}
    </div>
  );
}

export function EmptyState({
  title,
  description,
  action,
  icon,
}: {
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  icon?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-3 px-6 py-12 text-center">
      <div className="text-text-subtle">{icon ?? <Inbox size={28} />}</div>
      <div className="text-sm font-medium text-text">{title}</div>
      {description ? (
        <p className="max-w-sm text-sm text-text-subtle">{description}</p>
      ) : null}
      {action}
    </div>
  );
}

export function ErrorBox({
  message,
  onRetry,
  retryLabel = "Retry",
}: {
  message: string;
  onRetry?: () => void;
  retryLabel?: string;
}) {
  return (
    <div
      className="flex items-start gap-3 rounded-md border border-danger/30 bg-danger-soft px-4 py-3 text-sm text-danger"
      role="alert"
    >
      <AlertTriangle size={18} className="mt-0.5 shrink-0" />
      <div className="min-w-0 flex-1">
        <p className="break-words">{message}</p>
        {onRetry ? (
          <Button
            size="sm"
            variant="secondary"
            className="mt-2"
            icon={<RotateCw size={14} />}
            onClick={onRetry}
          >
            {retryLabel}
          </Button>
        ) : null}
      </div>
    </div>
  );
}

export type AlertTone = "info" | "warning" | "success" | "danger";

const alertTones: Record<AlertTone, string> = {
  info: "border-info/30 bg-info-soft text-info",
  warning: "border-warning/30 bg-warning-soft text-warning",
  success: "border-success/30 bg-success-soft text-success",
  danger: "border-danger/30 bg-danger-soft text-danger",
};

export function Alert({
  tone = "info",
  children,
  className,
}: {
  tone?: AlertTone;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "rounded-md border px-4 py-3 text-sm",
        alertTones[tone],
        className,
      )}
    >
      {children}
    </div>
  );
}
