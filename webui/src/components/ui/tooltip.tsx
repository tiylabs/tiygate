import type { ReactNode } from "react";
import * as RTooltip from "@radix-ui/react-tooltip";

export const TooltipProvider = RTooltip.Provider;

interface TooltipProps {
  content: ReactNode;
  children: ReactNode;
  side?: "top" | "right" | "bottom" | "left";
}

/** Lightweight tooltip for auxiliary explanations only (docs §6.8). */
export function Tooltip({ content, children, side = "top" }: TooltipProps) {
  return (
    <RTooltip.Root>
      <RTooltip.Trigger asChild>{children}</RTooltip.Trigger>
      <RTooltip.Portal>
        <RTooltip.Content
          side={side}
          sideOffset={6}
          className="z-40 max-w-xs rounded-md border border-border bg-surface px-2.5 py-1.5 text-xs text-text shadow-md data-[state=delayed-open]:animate-overlay-in"
        >
          {content}
          <RTooltip.Arrow className="fill-[var(--surface)]" />
        </RTooltip.Content>
      </RTooltip.Portal>
    </RTooltip.Root>
  );
}
