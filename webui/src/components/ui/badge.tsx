import type { PropsWithChildren } from "react";
import { cn } from "@/lib/cn";

export type BadgeTone =
  | "success"
  | "warning"
  | "danger"
  | "info"
  | "neutral"
  | "primary";

const tones: Record<BadgeTone, string> = {
  success: "bg-success-soft text-success",
  warning: "bg-warning-soft text-warning",
  danger: "bg-danger-soft text-danger",
  info: "bg-info-soft text-info",
  neutral: "bg-surface-muted text-text-muted",
  primary: "bg-primary-soft text-primary",
};

export function Badge({
  tone = "neutral",
  className,
  title,
  children,
}: PropsWithChildren<{
  tone?: BadgeTone;
  className?: string;
  /** Optional native tooltip, e.g. raw backend enum value. */
  title?: string;
}>) {
  return (
    <span
      title={title}
      className={cn(
        "inline-flex items-center gap-1 rounded-xs px-2 py-0.5 text-xs font-medium",
        tones[tone],
        className,
      )}
    >
      {children}
    </span>
  );
}
