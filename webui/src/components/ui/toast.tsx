import {
  createContext,
  useCallback,
  useContext,
  useMemo,
  useState,
  type PropsWithChildren,
} from "react";
import * as RToast from "@radix-ui/react-toast";
import { CheckCircle2, Info, X, XCircle } from "lucide-react";
import { cn } from "@/lib/cn";

export type ToastTone = "success" | "error" | "info";

interface ToastItem {
  id: number;
  tone: ToastTone;
  title: string;
  description?: string;
}

interface ToastApi {
  toast: (input: {
    tone?: ToastTone;
    title: string;
    description?: string;
  }) => void;
  success: (title: string, description?: string) => void;
  error: (title: string, description?: string) => void;
  info: (title: string, description?: string) => void;
}

const ToastContext = createContext<ToastApi | null>(null);

const toneStyles: Record<ToastTone, string> = {
  success: "border-success/40",
  error: "border-danger/40",
  info: "border-info/40",
};

const toneIcon: Record<ToastTone, typeof Info> = {
  success: CheckCircle2,
  error: XCircle,
  info: Info,
};

const toneIconColor: Record<ToastTone, string> = {
  success: "text-success",
  error: "text-danger",
  info: "text-info",
};

let nextId = 1;

export function ToastProvider({ children }: PropsWithChildren) {
  const [items, setItems] = useState<ToastItem[]>([]);

  const remove = useCallback((id: number) => {
    setItems((prev) => prev.filter((t) => t.id !== id));
  }, []);

  const push = useCallback(
    (input: { tone?: ToastTone; title: string; description?: string }) => {
      const id = nextId++;
      setItems((prev) => [
        ...prev,
        { id, tone: input.tone ?? "info", title: input.title, description: input.description },
      ]);
    },
    [],
  );

  const api = useMemo<ToastApi>(
    () => ({
      toast: push,
      success: (title, description) => push({ tone: "success", title, description }),
      error: (title, description) => push({ tone: "error", title, description }),
      info: (title, description) => push({ tone: "info", title, description }),
    }),
    [push],
  );

  return (
    <ToastContext.Provider value={api}>
      <RToast.Provider swipeDirection="right">
        {children}
        {items.map((item) => {
          const Icon = toneIcon[item.tone];
          // Errors persist longer; success/info auto-dismiss in 4s (docs §6.9).
          const duration = item.tone === "error" ? 8000 : 4000;
          return (
            <RToast.Root
              key={item.id}
              duration={duration}
              onOpenChange={(open) => {
                if (!open) remove(item.id);
              }}
              className={cn(
                "animate-toast-in flex items-start gap-3 rounded-lg border bg-surface px-4 py-3 shadow-md",
                toneStyles[item.tone],
              )}
            >
              <Icon size={18} className={cn("mt-0.5 shrink-0", toneIconColor[item.tone])} />
              <div className="min-w-0 flex-1">
                <RToast.Title className="text-sm font-medium text-text">
                  {item.title}
                </RToast.Title>
                {item.description ? (
                  <RToast.Description className="mt-0.5 break-words text-xs text-text-muted">
                    {item.description}
                  </RToast.Description>
                ) : null}
              </div>
              <RToast.Close
                aria-label="Close"
                className="-mr-1 rounded p-0.5 text-text-subtle transition-colors hover:bg-surface-muted hover:text-text"
              >
                <X size={14} />
              </RToast.Close>
            </RToast.Root>
          );
        })}
        <RToast.Viewport className="fixed bottom-0 right-0 z-[100] m-0 flex w-[calc(100%-2rem)] max-w-sm list-none flex-col gap-2 p-4 outline-none sm:right-4" />
      </RToast.Provider>
    </ToastContext.Provider>
  );
}

export function useToast(): ToastApi {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error("useToast must be used within ToastProvider");
  return ctx;
}
