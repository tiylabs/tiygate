import { useCallback, useEffect, useRef, useState, type MouseEvent } from "react";
import { ChevronRight, ChevronsDownUp, ChevronsUpDown } from "lucide-react";
import { cn } from "@/lib/cn";

/**
 * JsonViewer — a zero-dependency recursive JSON tree renderer.
 *
 * Renders a JSON-serialised string as a collapsible, syntax-coloured tree.
 * When the input fails to parse as JSON it falls back to a plain `<pre>`
 * block so non-JSON bodies still render usefully (matching the previous
 * behaviour of `MessageBlock` / `SseParsedBlock`).
 *
 * Visual language mirrors the existing `<details>`/`<summary>` + ChevronRight
 * pattern used throughout the request-log detail view, and colours come from
 * the semantic design tokens defined in `index.css`.
 */

export interface JsonViewerProps {
  /** A JSON-serialised string (or arbitrary text that falls back to `<pre>`). */
  value: string | null | undefined;
  className?: string;
}

/** Sentinel used to drive every descendant node into the expanded state. */
const EXPAND_ALL = Symbol("expand-all");
const COLLAPSE_ALL = Symbol("collapse-all");
type ExpandCommand = typeof EXPAND_ALL | typeof COLLAPSE_ALL | null;

export function JsonViewer({ value, className }: JsonViewerProps) {
  // Empty / null / undefined — nothing to render.
  if (value == null || value === "") return null;

  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch {
    // Non-JSON content — fall back to a plain `<pre>` so the raw text is
    // still visible (matches the previous behaviour before JsonViewer).
    return (
      <pre
        className={cn(
          "max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text",
          className,
        )}
      >
        {value}
      </pre>
    );
  }

  return <JsonTree data={parsed} className={className} />;
}

function JsonTree({ data, className }: { data: unknown; className?: string }) {
  // A monotonically increasing counter. Bumping it (via a command symbol)
  // forces every `JsonNode` to re-derive its collapsed state from the latest
  // command, giving us global expand-all / collapse-all without walking a ref
  // tree.
  const [command, setCommand] = useState<ExpandCommand>(null);
  const [counter, setCounter] = useState(0);
  const applyCommand = useCallback((cmd: ExpandCommand) => {
    setCommand(cmd);
    setCounter((c) => c + 1);
  }, []);

  const canExpand = isContainer(data);

  return (
    <div
      className={cn(
        "relative max-h-64 overflow-auto rounded-md bg-surface-muted p-3 font-mono text-xs text-text",
        className,
      )}
    >
      {canExpand ? (
        <div className="sticky top-0 z-10 float-right flex items-center gap-0.5 -mt-1 -mr-1">
          <ToolbarButton
            label="Expand all"
            icon={<ChevronsUpDown size={12} />}
            onClick={() => applyCommand(EXPAND_ALL)}
          />
          <ToolbarButton
            label="Collapse all"
            icon={<ChevronsDownUp size={12} />}
            onClick={() => applyCommand(COLLAPSE_ALL)}
          />
        </div>
      ) : null}
      <JsonNode data={data} depth={0} command={command} counter={counter} />
    </div>
  );
}

interface JsonNodeProps {
  data: unknown;
  depth: number;
  command: ExpandCommand;
  counter: number;
  keyName?: string;
  /** When true this node is the trailing element of an object/array line. */
  isLast?: boolean;
}

function JsonNode({ data, depth, command, counter, keyName, isLast = true }: JsonNodeProps) {
  if (data === null) {
    return <PrimitiveLine keyName={keyName} isLast={isLast} value={<span className="text-text-subtle">null</span>} />;
  }
  if (typeof data === "boolean") {
    return <PrimitiveLine keyName={keyName} isLast={isLast} value={<span className="text-warning">{String(data)}</span>} />;
  }
  if (typeof data === "number") {
    return <PrimitiveLine keyName={keyName} isLast={isLast} value={<span className="text-info">{String(data)}</span>} />;
  }
  if (typeof data === "string") {
    return <PrimitiveLine keyName={keyName} isLast={isLast} value={<span className="text-success">{JSON.stringify(data)}</span>} />;
  }
  // object or array — after the guards above, `data` is a non-null object
  const container = data as Record<string, unknown> | unknown[];
  return (
    <ContainerNode
      data={container}
      depth={depth}
      command={command}
      counter={counter}
      keyName={keyName}
      isLast={isLast}
    />
  );
}

function PrimitiveLine({
  keyName,
  isLast,
  value,
}: {
  keyName?: string;
  isLast: boolean;
  value: React.ReactNode;
}) {
  return (
    <div className="whitespace-pre break-all">
      {keyName !== undefined ? (
        <>
          <span className="text-primary">{JSON.stringify(keyName)}</span>
          <span className="text-text-subtle">: </span>
        </>
      ) : null}
      {value}
      {isLast ? null : <span className="text-text-subtle">,</span>}
    </div>
  );
}

function ContainerNode({
  data,
  depth,
  command,
  counter,
  keyName,
  isLast,
}: {
  data: Record<string, unknown> | unknown[];
  depth: number;
  command: ExpandCommand;
  counter: number;
  keyName?: string;
  isLast: boolean;
}) {
  const isArray = Array.isArray(data);
  const entries = isArray
    ? (data as unknown[]).map((v, i) => [String(i), v] as const)
    : Object.entries(data as Record<string, unknown>);

  const count = entries.length;
  const openBracket = isArray ? "[" : "{";
  const closeBracket = isArray ? "]" : "}";
  const summary = isArray ? `${count} item${count === 1 ? "" : "s"}` : `${count} key${count === 1 ? "" : "s"}`;

  // Depth > 1 nodes start collapsed; the first two levels stay open so the
  // user immediately sees structure without a wall of nested brackets.
  const defaultCollapsed = depth > 1;
  const [collapsed, setCollapsed] = useState(defaultCollapsed);

  // Honour global expand-all / collapse-all commands. Each command bumps the
  // counter, so this effect fires exactly once per command.
  const lastCounter = useRef(counter);
  useEffect(() => {
    if (counter === lastCounter.current) return;
    lastCounter.current = counter;
    if (command === EXPAND_ALL) setCollapsed(false);
    else if (command === COLLAPSE_ALL) setCollapsed(true);
  }, [command, counter]);

  function toggle(e: MouseEvent) {
    e.stopPropagation();
    setCollapsed((c) => !c);
  }

  return (
    <div className="break-all">
      <div className="flex items-start gap-0.5">
        <button
          type="button"
          onClick={toggle}
          aria-label={collapsed ? "Expand" : "Collapse"}
          className="mt-0.5 inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded text-text-subtle hover:text-text"
        >
          <ChevronRight
            size={12}
            className={cn("transition-transform", !collapsed && "rotate-90")}
          />
        </button>
        <div className="min-w-0">
          {keyName !== undefined ? (
            <>
              <span className="text-primary">{JSON.stringify(keyName)}</span>
              <span className="text-text-subtle">: </span>
            </>
          ) : null}
          <button
            type="button"
            onClick={toggle}
            className="cursor-pointer text-text-subtle hover:text-text"
          >
            <span>{openBracket}</span>
          </button>
          {collapsed ? (
            <span className="text-text-subtle"> {summary} </span>
          ) : null}
          <span>{collapsed ? closeBracket : ""}</span>
          {!collapsed && count === 0 ? (
            <>
              <span className="text-text-subtle">{closeBracket}</span>
              {isLast ? null : <span className="text-text-subtle">,</span>}
            </>
          ) : null}
          {collapsed ? (
            <>
              {isLast ? null : <span className="text-text-subtle">,</span>}
            </>
          ) : null}
        </div>
      </div>
      {!collapsed && count > 0 ? (
        <div className="ml-3.5 border-l border-border pl-2">
          {entries.map(([k, v], i) => (
            <JsonNode
              key={k}
              data={v}
              depth={depth + 1}
              command={command}
              counter={counter}
              keyName={isArray ? undefined : k}
              isLast={i === count - 1}
            />
          ))}
          <div>
            <span className="text-text-subtle">{closeBracket}</span>
            {isLast ? null : <span className="text-text-subtle">,</span>}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function ToolbarButton({
  label,
  icon,
  onClick,
  disabled,
}: {
  label: string;
  icon: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={label}
      title={label}
      disabled={disabled}
      className="inline-flex h-5 w-5 items-center justify-center rounded text-text-subtle transition-colors hover:bg-surface hover:text-text disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent"
    >
      {icon}
    </button>
  );
}

function isContainer(data: unknown): boolean {
  return data !== null && typeof data === "object";
}
