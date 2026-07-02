import {
  forwardRef,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type InputHTMLAttributes,
} from "react";
import { createPortal } from "react-dom";
import { DismissableLayerBranch } from "@radix-ui/react-dismissable-layer";
import { cn } from "@/lib/cn";

const fieldBase =
  "w-full rounded-sm border border-border-strong bg-surface px-3 py-1.5 text-sm " +
  "text-text placeholder:text-text-subtle " +
  "transition-colors duration-[var(--duration-fast)] " +
  "focus-visible:outline-none focus-visible:border-primary " +
  "focus-visible:ring-2 focus-visible:ring-ring " +
  "disabled:cursor-not-allowed disabled:opacity-50";

export interface ComboboxOption {
  value: string;
  label: string;
}

export interface ComboboxProps extends Omit<
  InputHTMLAttributes<HTMLInputElement>,
  "onChange"
> {
  options: ComboboxOption[];
  loading?: boolean;
  onChange?: (value: string) => void;
}

/**
 * Combobox — a searchable dropdown that also allows free-text input.
 *
 * - Type to filter the option list (case-insensitive substring match).
 * - Arrow keys to navigate, Enter to select, Escape to close.
 * - Values not in `options` are accepted (free input).
 * - Click outside or blur closes the dropdown.
 * - `loading` shows a spinner inside the input.
 * - The dropdown list is rendered via a Portal at `document.body` level
 *   with `fixed` positioning so it is never clipped by ancestor
 *   `overflow` containers (e.g. Dialog scroll areas).
 */
export const Combobox = forwardRef<HTMLInputElement, ComboboxProps>(
  function Combobox(
    {
      options,
      loading = false,
      onChange,
      className,
      value,
      onFocus,
      onBlur,
      onKeyDown,
      ...props
    },
    ref,
  ) {
    const containerRef = useRef<HTMLDivElement>(null);
    const inputRef = useRef<HTMLInputElement>(null);
    const listRef = useRef<HTMLDivElement>(null);
    // Flag: while true, blur events should NOT close the dropdown because
    // a suggestion click is in progress. This is necessary because the
    // dropdown is rendered in a Portal at document.body, so the input
    // blurs before the click would register on the portal element.
    const selectingRef = useRef(false);
    const [open, setOpen] = useState(false);
    const [highlighted, setHighlighted] = useState(-1);
    const [panelStyle, setPanelStyle] = useState<
      Record<string, string | number>
    >({});
    const listboxId = useId();

    // Forward the external ref to the internal input.
    useEffect(() => {
      if (typeof ref === "function") ref(inputRef.current);
      else if (ref)
        (ref as React.MutableRefObject<HTMLInputElement | null>).current =
          inputRef.current;
    }, [ref]);

    const filtered = open
      ? options.filter((opt) => {
          const v = (value as string) ?? "";
          return opt.value.toLowerCase().includes(v.toLowerCase());
        })
      : options;

    // Reset highlight when the filtered list changes.
    useLayoutEffect(() => {
      setHighlighted((h) =>
        filtered.length === 0 ? -1 : Math.min(h, filtered.length - 1),
      );
    }, [filtered.length]);

    // Compute dropdown position relative to the input element whenever
    // the dropdown opens. The panel is fixed-positioned in a portal so
    // it is immune to ancestor overflow clipping.
    useLayoutEffect(() => {
      if (!open || !inputRef.current) return;
      const rect = inputRef.current.getBoundingClientRect();
      setPanelStyle({
        position: "fixed",
        top: rect.bottom + 4,
        left: rect.left,
        width: rect.width,
      });
    }, [open]);

    // Recompute on scroll/resize while open (the input may move with
    // the scroll container).
    useEffect(() => {
      if (!open) return;
      function update() {
        if (!inputRef.current) return;
        const rect = inputRef.current.getBoundingClientRect();
        setPanelStyle({
          position: "fixed",
          top: rect.bottom + 4,
          left: rect.left,
          width: rect.width,
        });
      }
      window.addEventListener("scroll", update, true);
      window.addEventListener("resize", update);
      return () => {
        window.removeEventListener("scroll", update, true);
        window.removeEventListener("resize", update);
      };
    }, [open]);

    // Close on click outside.
    useEffect(() => {
      if (!open) return;
      function handler(e: MouseEvent) {
        const target = e.target as Node;
        if (
          containerRef.current?.contains(target) ||
          listRef.current?.contains(target)
        ) {
          return;
        }
        setOpen(false);
      }
      document.addEventListener("mousedown", handler);
      return () => document.removeEventListener("mousedown", handler);
    }, [open]);

    // Scroll highlighted item into view.
    useEffect(() => {
      if (!open || highlighted < 0) return;
      const list = listRef.current;
      if (!list) return;
      const item = list.children[highlighted] as HTMLElement | undefined;
      if (item) {
        item.scrollIntoView({ block: "nearest" });
      }
    }, [highlighted, open]);

    const selectOption = useCallback(
      (opt: ComboboxOption) => {
        selectingRef.current = true;
        onChange?.(opt.value);
        setOpen(false);
        setHighlighted(-1);
        // Restore focus to the input after the selection is committed.
        requestAnimationFrame(() => {
          inputRef.current?.focus();
          selectingRef.current = false;
        });
      },
      [onChange],
    );

    const handleFocus = (e: React.FocusEvent<HTMLInputElement>) => {
      setOpen(true);
      onFocus?.(e);
    };

    const handleBlur = (e: React.FocusEvent<HTMLInputElement>) => {
      // If a suggestion click is in progress, do not close — the
      // selectOption handler will manage open state and refocus.
      if (selectingRef.current) return;
      // Defer close so click on a suggestion fires before blur.
      setTimeout(() => {
        if (!selectingRef.current) setOpen(false);
      }, 120);
      onBlur?.(e);
    };

    const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        if (!open) {
          setOpen(true);
          return;
        }
        setHighlighted((h) => (h < filtered.length - 1 ? h + 1 : h));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setHighlighted((h) => (h > 0 ? h - 1 : 0));
      } else if (e.key === "Enter") {
        if (open && highlighted >= 0 && highlighted < filtered.length) {
          e.preventDefault();
          selectOption(filtered[highlighted]);
        }
      } else if (e.key === "Escape") {
        if (open) {
          e.preventDefault();
          setOpen(false);
        }
      }
      onKeyDown?.(e);
    };

    return (
      <div ref={containerRef} className="relative">
        <input
          ref={inputRef}
          className={cn(fieldBase, loading && "pr-8", className)}
          role="combobox"
          aria-expanded={open}
          aria-controls={listboxId}
          aria-autocomplete="list"
          aria-activedescendant={
            open && highlighted >= 0 ? `${listboxId}-${highlighted}` : undefined
          }
          value={value as string}
          onChange={(e) => onChange?.(e.target.value)}
          onFocus={handleFocus}
          onBlur={handleBlur}
          onKeyDown={handleKeyDown}
          {...props}
        />
        {loading && (
          <div
            className="pointer-events-none absolute inset-y-0 right-0 flex items-center pr-2"
            aria-hidden
          >
            <div className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-border border-t-primary" />
          </div>
        )}
        {open &&
          filtered.length > 0 &&
          typeof document !== "undefined" &&
          createPortal(
            <DismissableLayerBranch>
              <div
                ref={listRef}
                id={listboxId}
                role="listbox"
                style={{ ...panelStyle, pointerEvents: "auto" }}
                className="z-50 max-h-60 overflow-auto rounded-sm border border-border-strong bg-surface py-1 shadow-lg"
              >
                {filtered.map((opt, i) => (
                  <div
                    key={opt.value}
                    id={`${listboxId}-${i}`}
                    role="option"
                    aria-selected={i === highlighted}
                    className={cn(
                      "cursor-pointer px-3 py-1.5 text-sm",
                      i === highlighted
                        ? "bg-primary/10 text-primary"
                        : "text-text hover:bg-surface-muted",
                    )}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      selectOption(opt);
                    }}
                    onMouseEnter={() => setHighlighted(i)}
                  >
                    <span className="font-mono text-[12px]">{opt.label}</span>
                  </div>
                ))}
              </div>
            </DismissableLayerBranch>,
            document.body,
          )}
      </div>
    );
  },
);
