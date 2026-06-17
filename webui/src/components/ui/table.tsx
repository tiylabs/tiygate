import type { PropsWithChildren, RefObject } from "react";
import { cn } from "@/lib/cn";

export function Table({
  children,
  className,
  maxHeight,
  tableClassName,
  containerRef,
}: PropsWithChildren<{
  className?: string;
  /** Cap the inner scroll container so the sticky thead has a scrolling ancestor. */
  maxHeight?: string | string[];
  /** Classes applied to the inner <table> element (e.g. min-w-max for horizontal overflow). */
  tableClassName?: string;
  /** Ref to the scroll container div, for scroll-position tracking. */
  containerRef?: RefObject<HTMLDivElement>;
}>) {
  return (
    <div ref={containerRef} className={cn("overflow-auto", maxHeight, className)}>
      <table className={cn("w-full border-collapse text-sm", tableClassName)}>
        {children}
      </table>
    </div>
  );
}

/** Sticky table header that follows vertical scroll (docs §3.2). */
export function Thead({
  children,
  className,
}: PropsWithChildren<{ className?: string }>) {
  return (
    <thead
      className={cn("sticky top-0 z-20", className)}
    >
      {children}
    </thead>
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
        "group transition-colors duration-[var(--duration-fast)] hover:bg-surface-muted",
        className,
      )}
    >
      {children}
    </tr>
  );
}
