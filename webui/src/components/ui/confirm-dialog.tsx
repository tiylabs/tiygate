import type { ReactNode } from "react";
import * as RDialog from "@radix-ui/react-dialog";
import { AlertTriangle } from "lucide-react";
import { Button } from "./button";

interface ConfirmDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  /** Clear description of object name, impact and irreversibility. */
  description: ReactNode;
  confirmLabel: string;
  cancelLabel: string;
  /** Use danger styling + warning icon for destructive actions. */
  destructive?: boolean;
  loading?: boolean;
  onConfirm: () => void;
}

/**
 * Accessible second-confirmation dialog replacing window.confirm.
 * Built on Radix Dialog for focus management and Escape handling.
 */
export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirmLabel,
  cancelLabel,
  destructive = false,
  loading = false,
  onConfirm,
}: ConfirmDialogProps) {
  return (
    <RDialog.Root open={open} onOpenChange={onOpenChange}>
      <RDialog.Portal>
        <RDialog.Overlay className="animate-overlay-in fixed inset-0 z-40 bg-overlay backdrop-blur-[1px]" />
        <RDialog.Content
          onEscapeKeyDown={destructive ? (e) => e.preventDefault() : undefined}
          className="animate-content-in fixed left-1/2 top-1/2 z-50 w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 rounded-lg border border-border bg-surface shadow-lg focus:outline-none"
        >
          <div className="flex gap-3 px-5 py-5">
            {destructive ? (
              <div className="mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-danger-soft text-danger">
                <AlertTriangle size={18} />
              </div>
            ) : null}
            <div className="min-w-0 flex-1">
              <RDialog.Title className="text-base font-semibold tracking-[-0.01em] text-text">
                {title}
              </RDialog.Title>
              <RDialog.Description className="mt-1.5 text-sm text-text-muted">
                {description}
              </RDialog.Description>
            </div>
          </div>
          <div className="flex justify-end gap-2 border-t border-border px-5 py-3">
            <RDialog.Close asChild>
              <Button variant="secondary" disabled={loading}>
                {cancelLabel}
              </Button>
            </RDialog.Close>
            <Button
              variant={destructive ? "danger" : "primary"}
              loading={loading}
              onClick={onConfirm}
            >
              {confirmLabel}
            </Button>
          </div>
        </RDialog.Content>
      </RDialog.Portal>
    </RDialog.Root>
  );
}
