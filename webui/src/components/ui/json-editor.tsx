import { useEffect, useMemo, useRef, useState } from "react";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import {
  defaultHighlightStyle,
  foldGutter,
  indentOnInput,
  syntaxHighlighting,
} from "@codemirror/language";
import { json, jsonParseLinter } from "@codemirror/lang-json";
import { lintGutter, linter } from "@codemirror/lint";
import { Check, Copy } from "lucide-react";
import { cn } from "@/lib/cn";

interface JsonEditorProps {
  value: string;
  onChange: (value: string) => void;
  className?: string;
  ariaLabel?: string;
  copyLabel?: string;
  copiedLabel?: string;
}

const editorTheme = EditorView.theme({
  "&": {
    height: "14rem",
    color: "var(--color-text)",
    backgroundColor: "var(--color-surface)",
    fontSize: "12px",
    borderRadius: "0.375rem",
  },
  ".cm-scroller": {
    fontFamily:
      'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace',
    lineHeight: "1.5",
    overflow: "auto",
  },
  ".cm-content": {
    minHeight: "14rem",
    padding: "0.5rem 0",
  },
  ".cm-line": {
    padding: "0 0.75rem",
  },
  ".cm-gutters": {
    backgroundColor: "var(--color-surface-muted)",
    color: "var(--color-text-subtle)",
    borderRight: "1px solid var(--color-border)",
  },
  ".cm-activeLine": {
    backgroundColor: "color-mix(in srgb, var(--color-primary) 8%, transparent)",
  },
  ".cm-activeLineGutter": {
    backgroundColor: "color-mix(in srgb, var(--color-primary) 12%, transparent)",
  },
  ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": {
    backgroundColor: "color-mix(in srgb, var(--color-primary) 25%, transparent)",
  },
  "&.cm-focused": {
    outline: "2px solid color-mix(in srgb, var(--color-primary) 25%, transparent)",
    outlineOffset: "0",
  },
  ".cm-tooltip": {
    backgroundColor: "var(--color-surface)",
    border: "1px solid var(--color-border)",
    color: "var(--color-text)",
  },
  ".cm-diagnostic": {
    fontSize: "12px",
  },
});

export function JsonEditor({
  value,
  onChange,
  className,
  ariaLabel,
  copyLabel = "Copy",
  copiedLabel = "Copied",
}: JsonEditorProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<EditorView | null>(null);
  const onChangeRef = useRef(onChange);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    onChangeRef.current = onChange;
  }, [onChange]);

  const extensions = useMemo<Extension[]>(
    () => [
      history(),
      lineNumbers(),
      foldGutter(),
      lintGutter(),
      indentOnInput(),
      json(),
      linter(jsonParseLinter()),
      syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
      keymap.of([...defaultKeymap, ...historyKeymap]),
      editorTheme,
      EditorView.lineWrapping,
      EditorView.updateListener.of((update) => {
        if (update.docChanged) {
          onChangeRef.current(update.state.doc.toString());
        }
      }),
      EditorView.editorAttributes.of({
        "aria-label": ariaLabel ?? "JSON editor",
      }),
    ],
    [ariaLabel],
  );

  useEffect(() => {
    if (!hostRef.current || viewRef.current) return;
    const state = EditorState.create({ doc: value, extensions });
    const view = new EditorView({ state, parent: hostRef.current });
    viewRef.current = view;
    return () => {
      view.destroy();
      viewRef.current = null;
    };
  // Re-mount only when extensions change; `value` is intentionally excluded
  // so that keystrokes (which update `value`) do not destroy & recreate the
  // EditorView. External value updates are synced by the effect below.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [extensions]);

  useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    const current = view.state.doc.toString();
    if (current === value) return;
    view.dispatch({
      changes: { from: 0, to: current.length, insert: value },
    });
  }, [value]);

  async function copyValue() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      setCopied(false);
    }
  }

  return (
    <div
      className={cn(
        "relative overflow-hidden rounded-md border border-border bg-surface focus-within:border-primary",
        className,
      )}
    >
      <button
        type="button"
        className="absolute right-2 top-2 z-20 inline-flex h-7 w-7 items-center justify-center rounded-sm text-text-subtle transition hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        aria-label={copied ? copiedLabel : copyLabel}
        title={copied ? copiedLabel : copyLabel}
        onClick={copyValue}
      >
        {copied ? <Check size={14} /> : <Copy size={14} />}
      </button>
      <div ref={hostRef} />
    </div>
  );
}
