import type { PropsWithChildren, ReactNode } from "react";
import { cn } from "@/lib/cn";

export function Card({
  className,
  children,
}: PropsWithChildren<{ className?: string }>) {
  return (
    <div
      className={cn(
        "rounded-md border border-border bg-surface shadow-xs",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function CardHeader({
  title,
  description,
  action,
}: {
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-3 border-b border-border px-4 py-3">
      <div className="min-w-0">
        <h2 className="text-sm font-semibold text-text">{title}</h2>
        {description ? (
          <p className="mt-0.5 text-xs text-text-subtle">{description}</p>
        ) : null}
      </div>
      {action}
    </div>
  );
}

export function CardBody({
  className,
  children,
}: PropsWithChildren<{ className?: string }>) {
  return <div className={cn("p-4", className)}>{children}</div>;
}

/** Dashboard metric card: label + value + caption (statistic口径). */
export function Metric({
  label,
  value,
  caption,
  tone,
}: {
  label: ReactNode;
  value: ReactNode;
  caption?: ReactNode;
  tone?: "default" | "danger" | "success";
}) {
  const valueTone =
    tone === "danger"
      ? "text-danger"
      : tone === "success"
        ? "text-success"
        : "text-text";
  return (
    <Card className="p-4">
      <div className="text-label text-text-subtle">{label}</div>
      <div className={cn("text-display mt-1", valueTone)}>{value}</div>
      {caption ? (
        <div className="mt-1 text-xs text-text-subtle">{caption}</div>
      ) : null}
    </Card>
  );
}
