import type { ReactNode } from "react";
import * as RDropdown from "@radix-ui/react-dropdown-menu";
import { MoreHorizontal } from "lucide-react";
import { cn } from "@/lib/cn";

const contentClass =
  "z-40 min-w-[10rem] overflow-hidden rounded-md border border-border bg-surface p-1 shadow-md " +
  "data-[state=open]:animate-overlay-in";

const itemClass =
  "flex cursor-pointer items-center gap-2 rounded-sm px-2.5 py-1.5 text-sm text-text outline-none " +
  "transition-colors duration-[var(--duration-fast)] focus:bg-surface-muted data-[highlighted]:bg-surface-muted " +
  "data-[disabled]:pointer-events-none data-[disabled]:opacity-50";

export interface DropdownItem {
  key: string;
  label: ReactNode;
  icon?: ReactNode;
  onSelect: () => void;
  disabled?: boolean;
  /** Render in danger color and after a separator. */
  destructive?: boolean;
}

interface RowActionsProps {
  items: DropdownItem[];
  /** Accessible label for the icon trigger. */
  label: string;
}

/** Table row action menu (docs §6.7, §8.3). */
export function RowActions({ items, label }: RowActionsProps) {
  const normal = items.filter((i) => !i.destructive);
  const destructive = items.filter((i) => i.destructive);
  return (
    <RDropdown.Root>
      <RDropdown.Trigger asChild>
        <button
          aria-label={label}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-muted transition-colors duration-[var(--duration-fast)] hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <MoreHorizontal size={16} />
        </button>
      </RDropdown.Trigger>
      <RDropdown.Portal>
        <RDropdown.Content align="end" sideOffset={4} className={contentClass}>
          {normal.map((item) => (
            <RDropdown.Item
              key={item.key}
              disabled={item.disabled}
              onSelect={item.onSelect}
              className={itemClass}
            >
              {item.icon}
              {item.label}
            </RDropdown.Item>
          ))}
          {destructive.length > 0 && normal.length > 0 ? (
            <RDropdown.Separator className="my-1 h-px bg-border" />
          ) : null}
          {destructive.map((item) => (
            <RDropdown.Item
              key={item.key}
              disabled={item.disabled}
              onSelect={item.onSelect}
              className={cn(itemClass, "text-danger focus:bg-danger-soft data-[highlighted]:bg-danger-soft")}
            >
              {item.icon}
              {item.label}
            </RDropdown.Item>
          ))}
        </RDropdown.Content>
      </RDropdown.Portal>
    </RDropdown.Root>
  );
}
