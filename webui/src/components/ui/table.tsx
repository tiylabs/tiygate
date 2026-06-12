import type { PropsWithChildren } from "react";
import { cn } from "@/lib/cn";

export function Table({
  children,
  className,
}: PropsWithChildren<{ className?: string }>) {
  return (
    <div className="overflow-x-auto">
      <table className={cn("w-full border-collapse text-sm", className)}>
        {children}
      </table>
    </div>
  );
}

export function Th({
  children,
  className,
}: PropsWithChildren<{ className?: string }>) {
  return (
    <th
      className={cn(
        "text-label border-b border-border bg-surface-muted px-4 py-2.5 text-left text-text-muted",
        className,
      )}
    >
      {children}
    </th>
  );
}

export function Td({
  children,
  className,
  title,
}: PropsWithChildren<{ className?: string; title?: string }>) {
  return (
    <td
      title={title}
      className={cn("border-b border-border px-4 py-3 text-text", className)}
    >
      {children}
    </td>
  );
}

/** Table row with hover highlight (docs §3.2). */
export function Tr({
  children,
  className,
}: PropsWithChildren<{ className?: string }>) {
  return (
    <tr
      className={cn(
        "transition-colors duration-[var(--duration-fast)] hover:bg-surface-muted",
        className,
      )}
    >
      {children}
    </tr>
  );
}
