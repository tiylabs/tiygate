import type { PropsWithChildren, ReactNode } from "react";
import { cn } from "@/lib/cn";

export function Label({
  children,
  htmlFor,
  className,
}: PropsWithChildren<{ htmlFor?: string; className?: string }>) {
  return (
    <label
      htmlFor={htmlFor}
      className={cn("block text-sm font-medium text-text", className)}
    >
      {children}
    </label>
  );
}

export function Field({
  label,
  hint,
  error,
  required,
  children,
}: PropsWithChildren<{
  label: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  required?: boolean;
}>) {
  return (
    <div className="space-y-1.5">
      <Label>
        {label}
        {required ? <span className="ml-0.5 text-danger">*</span> : null}
      </Label>
      {children}
      {error ? (
        <p className="text-xs text-danger" role="alert">
          {error}
        </p>
      ) : hint ? (
        <p className="text-xs text-text-subtle">{hint}</p>
      ) : null}
    </div>
  );
}
