import type { ReactNode } from "react";
import * as RSelect from "@radix-ui/react-select";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "@/lib/cn";

export interface SelectOption {
  value: string;
  label: ReactNode;
}

/**
 * Radix Select forbids empty-string item values (it reserves "" to clear the
 * selection). We map "" to/from this sentinel internally so callers can keep
 * using "" to represent an unselected option.
 */
const EMPTY_VALUE = "__empty__";

interface SelectProps {
  value: string;
  onValueChange: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  /** aria-label when no visible <label> is associated. */
  ariaLabel?: string;
  disabled?: boolean;
  className?: string;
}

/** Accessible Radix Select replacing the native <select> (docs §6.7). */
export function Select({
  value,
  onValueChange,
  options,
  placeholder,
  ariaLabel,
  disabled,
  className,
}: SelectProps) {
  return (
    <RSelect.Root
      value={value === "" ? EMPTY_VALUE : value}
      onValueChange={(v) => onValueChange(v === EMPTY_VALUE ? "" : v)}
      disabled={disabled}
    >
      <RSelect.Trigger
        aria-label={ariaLabel}
        className={cn(
          "inline-flex w-full items-center justify-between gap-2 rounded-sm border border-border-strong bg-surface px-3 py-1.5 text-sm text-text transition-colors duration-[var(--duration-fast)]",
          "focus-visible:outline-none focus-visible:border-primary focus-visible:ring-2 focus-visible:ring-ring",
          "disabled:cursor-not-allowed disabled:opacity-50 data-[placeholder]:text-text-subtle",
          className,
        )}
      >
        <RSelect.Value placeholder={placeholder} />
        <RSelect.Icon>
          <ChevronDown size={16} className="text-text-subtle" />
        </RSelect.Icon>
      </RSelect.Trigger>
      <RSelect.Portal>
        <RSelect.Content
          position="popper"
          sideOffset={4}
          className="z-40 max-h-72 min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-md border border-border bg-surface shadow-md"
        >
          <RSelect.Viewport className="p-1">
            {options.map((opt) => (
              <RSelect.Item
                key={opt.value}
                value={opt.value === "" ? EMPTY_VALUE : opt.value}
                className="flex cursor-pointer items-center justify-between gap-2 rounded-sm px-2.5 py-1.5 text-sm text-text outline-none data-[highlighted]:bg-surface-muted data-[state=checked]:font-medium"
              >
                <RSelect.ItemText>{opt.label}</RSelect.ItemText>
                <RSelect.ItemIndicator>
                  <Check size={14} className="text-primary" />
                </RSelect.ItemIndicator>
              </RSelect.Item>
            ))}
          </RSelect.Viewport>
        </RSelect.Content>
      </RSelect.Portal>
    </RSelect.Root>
  );
}
