import {
  forwardRef,
  type ButtonHTMLAttributes,
  type ReactNode,
} from "react";
import { cn } from "@/lib/cn";

export type ButtonVariant =
  | "primary"
  | "secondary"
  | "ghost"
  | "danger"
  | "accent";
export type ButtonSize = "sm" | "md";

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  loading?: boolean;
  /** Optional leading icon node. Hidden while loading. */
  icon?: ReactNode;
}

const base =
  "inline-flex items-center justify-center gap-1.5 rounded-md font-medium " +
  "transition-colors cursor-pointer select-none " +
  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary " +
  "focus-visible:ring-offset-2 focus-visible:ring-offset-bg " +
  "disabled:cursor-not-allowed disabled:opacity-50 disabled:pointer-events-none";

const sizes: Record<ButtonSize, string> = {
  sm: "min-h-8 px-2.5 py-1 text-xs",
  md: "min-h-9 px-3.5 py-1.5 text-sm",
};

const variants: Record<ButtonVariant, string> = {
  primary:
    "bg-primary text-on-primary hover:bg-primary-strong active:bg-primary-strong",
  secondary:
    "bg-surface text-text border border-border hover:bg-surface-muted",
  ghost: "bg-transparent text-text-muted hover:bg-surface-muted hover:text-text",
  danger: "bg-danger text-on-danger hover:bg-danger-strong",
  accent: "bg-accent text-on-accent hover:bg-accent-strong",
};

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  function Button(
    {
      variant = "secondary",
      size = "md",
      loading = false,
      icon,
      className,
      children,
      disabled,
      ...props
    },
    ref,
  ) {
    return (
      <button
        ref={ref}
        className={cn(base, sizes[size], variants[variant], className)}
        disabled={disabled || loading}
        aria-busy={loading || undefined}
        {...props}
      >
        {loading ? (
          <span
            className="h-3.5 w-3.5 animate-spin rounded-full border-2 border-current border-t-transparent"
            aria-hidden
          />
        ) : (
          icon
        )}
        {children}
      </button>
    );
  },
);
