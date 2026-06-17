import type { ReactNode } from "react";
import * as RDialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { cn } from "@/lib/cn";

export type DialogSize = "sm" | "md" | "lg";

const sizes: Record<DialogSize, string> = {
  sm: "max-w-md",
  md: "max-w-lg",
  lg: "max-w-2xl",
};

interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  size?: DialogSize;
  /** Hide the title visually but keep it for screen readers. */
  hideTitle?: boolean;
  /** Close button label for a11y. */
  closeLabel?: string;
}

/**
 * Tokenized Radix Dialog wrapper. Handles focus trap/restore, Escape,
 * scroll lock and a11y title/description out of the box.
 */
export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  size = "md",
  hideTitle = false,
  closeLabel = "Close",
}: DialogProps) {
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className="animate-overlay-in fixed inset-0 z-40 bg-overlay backdrop-blur-[1px]" />
        <RDialog.Content
          className={cn(
            "animate-content-in fixed left-1/2 top-1/2 z-50 flex max-h-[85vh] w-[calc(100%-2rem)] -translate-x-1/2 -translate-y-1/2 flex-col rounded-lg border border-border bg-surface shadow-lg focus:outline-none",
            sizes[size],
          )}
        >
          <div className="flex items-start justify-between gap-4 border-b border-border px-5 py-4">
            <div className="min-w-0">
              {hideTitle ? (
                <RDialog.Title className="sr-only">{title}</RDialog.Title>
              ) : (
                <RDialog.Title className="text-base font-semibold tracking-[-0.01em] text-text">
                  {title}
                </RDialog.Title>
              )}
              {description ? (
                <RDialog.Description className="mt-1 text-sm text-text-muted">
                  {description}
                </RDialog.Description>
              ) : null}
            </div>
            <RDialog.Close
              aria-label={closeLabel}
              className="-mr-1 rounded-md p-1 text-text-subtle transition-colors duration-[var(--duration-fast)] hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <X size={18} />
            </RDialog.Close>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
            {children}
          </div>
          {footer ? (
            <div className="flex flex-wrap justify-end gap-2 border-t border-border px-5 py-3">
              {footer}
            </div>
          ) : null}
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  );
}

interface DrawerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  closeLabel?: string;
}

/** Right-side detail drawer built on Radix Dialog (docs §8.5). */
export function Drawer({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  closeLabel = "Close",
}: DrawerProps) {
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className="animate-overlay-in fixed inset-0 z-40 bg-overlay backdrop-blur-[1px]" />
        <RDialog.Content className="data-[state=open]:animate-drawer-right-in fixed right-0 top-0 z-50 flex h-full w-full max-w-xl flex-col border-l border-border bg-surface shadow-lg focus:outline-none">
          <div className="flex items-start justify-between gap-4 border-b border-border px-5 py-4">
            <div className="min-w-0">
              <RDialog.Title className="truncate text-base font-semibold tracking-[-0.01em] text-text">
                {title}
              </RDialog.Title>
              {description ? (
                <RDialog.Description className="mt-1 text-sm text-text-muted">
                  {description}
                </RDialog.Description>
              ) : null}
            </div>
            <RDialog.Close
              aria-label={closeLabel}
              className="-mr-1 rounded-md p-1 text-text-subtle transition-colors duration-[var(--duration-fast)] hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              <X size={18} />
            </RDialog.Close>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
            {children}
          </div>
          {footer ? (
            <div className="flex flex-wrap justify-end gap-2 border-t border-border px-5 py-3">
              {footer}
            </div>
          ) : null}
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  );
}
