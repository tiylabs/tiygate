import { useId } from "react";
import * as RSwitch from "@radix-ui/react-switch";

interface SwitchProps {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
  label: string;
  disabled?: boolean;
}

/** Accessible toggle replacing the enabled checkbox (docs §7 Switch). */
export function Switch({
  checked,
  onCheckedChange,
  label,
  disabled,
}: SwitchProps) {
  const id = useId();
  return (
    <div className="flex items-center gap-2.5">
      <RSwitch.Root
        id={id}
        checked={checked}
        onCheckedChange={onCheckedChange}
        disabled={disabled}
        className="relative h-5 w-9 shrink-0 cursor-pointer rounded-full border border-border-strong bg-surface-muted transition-colors duration-[var(--duration-fast)] data-[state=checked]:border-primary data-[state=checked]:bg-primary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-bg disabled:cursor-not-allowed disabled:opacity-50"
      >
        <RSwitch.Thumb className="block h-4 w-4 translate-x-0.5 rounded-full bg-surface shadow-xs transition-transform duration-[var(--duration-fast)] data-[state=checked]:translate-x-[18px] data-[state=checked]:bg-on-primary" />
      </RSwitch.Root>
      <label htmlFor={id} className="cursor-pointer text-sm text-text">
        {label}
      </label>
    </div>
  );
}
