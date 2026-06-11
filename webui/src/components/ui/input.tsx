import {
  forwardRef,
  useId,
  useState,
  type InputHTMLAttributes,
  type TextareaHTMLAttributes,
} from "react";
import { Eye, EyeOff } from "lucide-react";
import { cn } from "@/lib/cn";

const fieldBase =
  "w-full rounded-md border border-border bg-surface px-3 py-1.5 text-sm " +
  "text-text placeholder:text-text-subtle transition-colors " +
  "focus-visible:outline-none focus-visible:border-primary " +
  "focus-visible:ring-2 focus-visible:ring-primary/40 " +
  "disabled:cursor-not-allowed disabled:opacity-50";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(
  function Input({ className, ...props }, ref) {
    return <input ref={ref} className={cn(fieldBase, className)} {...props} />;
  },
);

export const Textarea = forwardRef<
  HTMLTextAreaElement,
  TextareaHTMLAttributes<HTMLTextAreaElement>
>(function Textarea({ className, ...props }, ref) {
  return (
    <textarea
      ref={ref}
      className={cn(fieldBase, "font-mono leading-relaxed", className)}
      {...props}
    />
  );
});

interface PasswordInputProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "type"> {
  /** aria-label for the show/hide toggle button. */
  toggleLabel?: string;
}

/** Password input with a show/hide visibility toggle (docs §12). */
export function PasswordInput({
  className,
  toggleLabel = "Toggle visibility",
  ...props
}: PasswordInputProps) {
  const [visible, setVisible] = useState(false);
  const id = useId();
  return (
    <div className="relative">
      <input
        id={id}
        type={visible ? "text" : "password"}
        className={cn(fieldBase, "pr-10", className)}
        {...props}
      />
      <button
        type="button"
        aria-label={toggleLabel}
        aria-pressed={visible}
        onClick={() => setVisible((v) => !v)}
        className="absolute inset-y-0 right-0 flex items-center px-3 text-text-subtle hover:text-text focus-visible:outline-none focus-visible:text-primary"
      >
        {visible ? <EyeOff size={16} /> : <Eye size={16} />}
      </button>
    </div>
  );
}
