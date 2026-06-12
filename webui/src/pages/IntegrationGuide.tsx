import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import {
  Activity,
  AlertTriangle,
  ArrowUpRight,
  BookOpen,
  Check,
  Code2,
  Copy,
  KeyRound,
  Radio,
  Server,
  ShieldAlert,
  TerminalSquare,
  Webhook,
  type LucideIcon,
} from "lucide-react";
import {
  Alert,
  Badge,
  Button,
  Card,
  CardBody,
  CardHeader,
  Field,
  Input,
  Select,
  Switch,
  TooltipProvider,
  useToast,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";
import { cn } from "@/lib/cn";
import openaiBrand from "@/assets/brand/openai.svg?raw";
import anthropicBrand from "@/assets/brand/anthropic.svg?raw";
import geminiBrand from "@/assets/brand/googlegemini.svg?raw";
import pythonLang from "@/assets/lang/python.svg?raw";
import typescriptLang from "@/assets/lang/typescript.svg?raw";

type ProtocolId = "openai" | "anthropic" | "responses" | "embeddings" | "gemini";
type LanguageId = "python" | "typescript" | "curl";

interface ProtocolSpec {
  id: ProtocolId;
  labelKey: string;
  pathKey: string;
  path: string;
  /** Path with `{model}` placeholder for templates. */
  pathTemplate: string;
  descriptionKey: string;
  icon: LucideIcon;
  /** Inline SVG markup. SVGs use `fill="currentColor"` so they pick up the
   *  surrounding `text-*` color, which is `text-primary` (light blue in light
   *  themes, soft blue in dark themes). */
  brand: string;
  brandLabel: string;
  auth: "bearer" | "x-api-key";
}

interface LanguageSpec {
  id: LanguageId;
  label: string;
  icon: LucideIcon;
  fileName: string;
  /** Inline SVG markup for the language badge. `null` keeps the Lucide icon
   *  (used for the generic cURL entry which has no brand mark). */
  brand: string | null;
  brandLabel: string | null;
}

const PROTOCOLS: ProtocolSpec[] = [
  {
    id: "openai",
    labelKey: "integration.protocolOpenai",
    pathKey: "integration.protocolOpenaiPath",
    path: "/v1/chat/completions",
    pathTemplate: "/v1/chat/completions",
    descriptionKey: "integration.protocolOpenaiDesc",
    icon: Radio,
    brand: openaiBrand,
    brandLabel: "OpenAI",
    auth: "bearer",
  },
  {
    id: "responses",
    labelKey: "integration.protocolResponses",
    pathKey: "integration.protocolResponsesPath",
    path: "/v1/responses",
    pathTemplate: "/v1/responses",
    descriptionKey: "integration.protocolResponsesDesc",
    icon: Webhook,
    brand: openaiBrand,
    brandLabel: "OpenAI",
    auth: "bearer",
  },
  {
    id: "anthropic",
    labelKey: "integration.protocolAnthropic",
    pathKey: "integration.protocolAnthropicPath",
    path: "/v1/messages",
    pathTemplate: "/v1/messages",
    descriptionKey: "integration.protocolAnthropicDesc",
    icon: BookOpen,
    brand: anthropicBrand,
    brandLabel: "Anthropic",
    auth: "x-api-key",
  },
  {
    id: "gemini",
    labelKey: "integration.protocolGemini",
    pathKey: "integration.protocolGeminiPath",
    path: "/v1beta/models/{model}:generateContent",
    pathTemplate: "/v1beta/models/{model}:generateContent",
    descriptionKey: "integration.protocolGeminiDesc",
    icon: Server,
    brand: geminiBrand,
    brandLabel: "Google Gemini",
    auth: "bearer",
  },
  {
    id: "embeddings",
    labelKey: "integration.protocolEmbeddings",
    pathKey: "integration.protocolEmbeddingsPath",
    path: "/v1/embeddings",
    pathTemplate: "/v1/embeddings",
    descriptionKey: "integration.protocolEmbeddingsDesc",
    icon: Code2,
    brand: openaiBrand,
    brandLabel: "OpenAI",
    auth: "bearer",
  },
];

const LANGUAGES: LanguageSpec[] = [
  {
    id: "python",
    label: "Python",
    icon: TerminalSquare,
    fileName: "sample.py",
    brand: pythonLang,
    brandLabel: "Python",
  },
  {
    id: "typescript",
    label: "TypeScript",
    icon: Code2,
    fileName: "sample.ts",
    brand: typescriptLang,
    brandLabel: "TypeScript",
  },
  {
    id: "curl",
    label: "cURL",
    icon: TerminalSquare,
    fileName: "sample.sh",
    brand: null,
    brandLabel: null,
  },
];

const DEFAULT_API_BASE = "http://localhost:3000";
const DEFAULT_VIRTUAL_MODEL = "gpt-4o-mini";
const STORAGE = {
  baseUrl: "tiygate.integration.baseUrl",
  model: "tiygate.integration.model",
  stream: "tiygate.integration.stream",
  protocol: "tiygate.integration.protocol",
  language: "tiygate.integration.language",
} as const;

function trimTrailingSlash(s: string): string {
  return s.endsWith("/") ? s.slice(0, -1) : s;
}

function escapeForPythonString(s: string): string {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function escapeForTypeScriptString(s: string): string {
  return s.replace(/\\/g, "\\\\").replace(/`/g, "\\`").replace(/\$\{/g, "\\${");
}

function escapeForCurl(s: string): string {
  return s.replace(/'/g, "'\\''");
}

function readStoredString(key: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  return window.localStorage.getItem(key) ?? fallback;
}

function readStoredBoolean(key: string, fallback: boolean): boolean {
  if (typeof window === "undefined") return fallback;
  const raw = window.localStorage.getItem(key);
  if (raw === null) return fallback;
  return raw === "1";
}

function protocolPathFor(protocol: ProtocolId, model: string): string {
  if (protocol === "gemini") {
    return `/v1beta/models/${encodeURIComponent(model || "model")}:generateContent`;
  }
  const p = PROTOCOLS.find((it) => it.id === protocol);
  return p ? p.path : "";
}

function pythonBodyFor(protocol: ProtocolId, model: string, stream: boolean): string {
  const s = stream ? "True" : "False";
  switch (protocol) {
    case "openai":
      return `{
    "model": "${escapeForPythonString(model)}",
    "stream": ${s},
    "messages": [
        {"role": "system", "content": "You are a helpful assistant."},
        {"role": "user", "content": "Hello, who are you?"},
    ],
}`;
    case "anthropic":
      return `{
    "model": "${escapeForPythonString(model)}",
    "max_tokens": 1024,
    "stream": ${s},
    "system": "You are a helpful assistant.",
    "messages": [
        {"role": "user", "content": "Hello, who are you?"},
    ],
}`;
    case "responses":
      return `{
    "model": "${escapeForPythonString(model)}",
    "stream": ${s},
    "input": "Hello, who are you?",
}`;
    case "embeddings":
      return `{
    "model": "${escapeForPythonString(model)}",
    "input": "Hello, who are you?",
}`;
    case "gemini":
      return `{
    "contents": [
        {
            "role": "user",
            "parts": [{"text": "Hello, who are you?"}],
        }
    ],
}`;
  }
}

function typescriptBodyFor(protocol: ProtocolId, model: string, stream: boolean): string {
  const s = stream ? "true" : "false";
  switch (protocol) {
    case "openai":
      return `{
  model: ${JSON.stringify(model)},
  stream: ${s},
  messages: [
    { role: "system", content: "You are a helpful assistant." },
    { role: "user", content: "Hello, who are you?" },
  ],
}`;
    case "anthropic":
      return `{
  model: ${JSON.stringify(model)},
  max_tokens: 1024,
  stream: ${s},
  system: "You are a helpful assistant.",
  messages: [
    { role: "user", content: "Hello, who are you?" },
  ],
}`;
    case "responses":
      return `{
  model: ${JSON.stringify(model)},
  stream: ${s},
  input: "Hello, who are you?",
}`;
    case "embeddings":
      return `{
  model: ${JSON.stringify(model)},
  input: "Hello, who are you?",
}`;
    case "gemini":
      return `{
  contents: [
    {
      role: "user",
      parts: [{ text: "Hello, who are you?" }],
    },
  ],
}`;
  }
}

function curlBodyFor(protocol: ProtocolId, model: string, stream: boolean): string {
  const lines: string[] = [];
  switch (protocol) {
    case "openai":
      lines.push(
        "{",
        `  "model": "${model}",`,
        `  "stream": ${stream},`,
        `  "messages": [`,
        `    {"role": "system", "content": "You are a helpful assistant."},`,
        `    {"role": "user", "content": "Hello, who are you?"}`,
        `  ]`,
        "}",
      );
      break;
    case "anthropic":
      lines.push(
        "{",
        `  "model": "${model}",`,
        `  "max_tokens": 1024,`,
        `  "stream": ${stream},`,
        `  "system": "You are a helpful assistant.",`,
        `  "messages": [`,
        `    {"role": "user", "content": "Hello, who are you?"}`,
        `  ]`,
        "}",
      );
      break;
    case "responses":
      lines.push(
        "{",
        `  "model": "${model}",`,
        `  "stream": ${stream},`,
        `  "input": "Hello, who are you?"`,
        "}",
      );
      break;
    case "embeddings":
      lines.push(
        "{",
        `  "model": "${model}",`,
        `  "input": "Hello, who are you?"`,
        "}",
      );
      break;
    case "gemini":
      lines.push(
        "{",
        `  "contents": [`,
        `    {`,
        `      "role": "user",`,
        `      "parts": [{"text": "Hello, who are you?"}]`,
        `    }`,
        `  ]`,
        "}",
      );
      break;
  }
  return lines
    .join("\n")
    .replace(/\n\s*/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

function buildPython(
  protocol: ProtocolId,
  baseUrl: string,
  model: string,
  apiKey: string,
  stream: boolean,
): string {
  const url = `${trimTrailingSlash(baseUrl)}${protocolPathFor(protocol, model)}`;
  const authValue =
    protocol === "anthropic"
      ? `{"x-api-key": "${escapeForPythonString(apiKey)}", "anthropic-version": "2023-06-01"}`
      : `{"Authorization": f"Bearer ${apiKey || "<YOUR_API_KEY>"}"}`;
  const body = pythonBodyFor(protocol, model, stream);
  const streamSnippet = stream
    ? `\n\ndef _print_stream(events):\n    for event in events:\n        sys.stdout.write(event)\n        sys.stdout.flush()\n\n\ndef main() -> None:\n    if STREAM:\n        with httpx.stream("POST", URL, headers=HEADERS, json=BODY, timeout=60.0) as response:\n            response.raise_for_status()\n            _print_stream(response.iter_text())\n        return\n    response = httpx.post(URL, headers=HEADERS, json=BODY, timeout=60.0)\n    response.raise_for_status()\n    print(response.text)\n\n\nif __name__ == "__main__":\n    main()\n`
    : `\n\ndef main() -> None:\n    response = httpx.post(URL, headers=HEADERS, json=BODY, timeout=60.0)\n    response.raise_for_status()\n    print(response.json())\n\n\nif __name__ == "__main__":\n    main()\n`;
  return `# ${protocol} — Python (pip install httpx)
import httpx${stream ? "\nimport sys" : ""}

URL = "${escapeForPythonString(url)}"
HEADERS = ${authValue}
BODY = ${body}
STREAM = ${stream ? "True" : "False"}${streamSnippet}`;
}

function buildTypeScript(
  protocol: ProtocolId,
  baseUrl: string,
  model: string,
  apiKey: string,
  stream: boolean,
): string {
  const url = `${trimTrailingSlash(baseUrl)}${protocolPathFor(protocol, model)}`;
  const isAnthropic = protocol === "anthropic";
  const body = typescriptBodyFor(protocol, model, stream);
  const requestSnippet = stream
    ? `const response = await fetch(URL, {
  method: "POST",
  headers: HEADERS,
  body: JSON.stringify(BODY),
});

if (!response.ok || !response.body) {
  throw new Error(\`HTTP \${response.status} \${response.statusText}\`);
}

const reader = response.body.getReader();
const decoder = new TextDecoder();
while (true) {
  const { value, done } = await reader.read();
  if (done) break;
  process.stdout.write(decoder.decode(value, { stream: true }));
}`
    : `const response = await fetch(URL, {
  method: "POST",
  headers: HEADERS,
  body: JSON.stringify(BODY),
});

if (!response.ok) {
  throw new Error(\`HTTP \${response.status} \${response.statusText}\`);
}

const data = await response.json();
console.log(data);`;
  const headersExpr = isAnthropic
    ? `{
  "x-api-key": ${apiKey ? `"${escapeForTypeScriptString(apiKey)}"` : `"<YOUR_API_KEY>"`},
  "anthropic-version": "2023-06-01",
  "Content-Type": "application/json",
}`
    : `{
  Authorization: \`Bearer ${apiKey || "<YOUR_API_KEY>"}\`,
  "Content-Type": "application/json",
}`;
  return `// ${protocol} — TypeScript (Node 18+)
const URL = ${JSON.stringify(url)};
const HEADERS = ${headersExpr};
const BODY = ${body};

${requestSnippet}
`;
}

function buildCurl(
  protocol: ProtocolId,
  baseUrl: string,
  model: string,
  apiKey: string,
  stream: boolean,
): string {
  const url = `${trimTrailingSlash(baseUrl)}${protocolPathFor(protocol, model)}`;
  const isAnthropic = protocol === "anthropic";
  const keyPlaceholder = apiKey || "<YOUR_API_KEY>";
  const authLine = isAnthropic
    ? `-H "x-api-key: ${escapeForCurl(keyPlaceholder)}" \\\n  -H "anthropic-version: 2023-06-01" \\`
    : `-H "Authorization: Bearer ${escapeForCurl(keyPlaceholder)}" \\`;
  const streamLine = stream ? " \\\n  -N" : "";
  const body = curlBodyFor(protocol, model, stream);
  return `# ${protocol} — cURL
curl -X POST "${escapeForCurl(url)}" \\
  -H "Content-Type: application/json" \\
  ${authLine}
  -d '${escapeForCurl(body)}'${streamLine}`;
}

function buildSample(
  language: LanguageId,
  protocol: ProtocolId,
  baseUrl: string,
  model: string,
  apiKey: string,
  stream: boolean,
): string {
  switch (language) {
    case "python":
      return buildPython(protocol, baseUrl, model, apiKey, stream);
    case "typescript":
      return buildTypeScript(protocol, baseUrl, model, apiKey, stream);
    case "curl":
      return buildCurl(protocol, baseUrl, model, apiKey, stream);
  }
}

/* -------------------------------------------------------------------------- */
/* Brand icon (inline SVG, inherits currentColor for dark mode)               */
/* -------------------------------------------------------------------------- */

/** Renders an inline SVG that picks up the surrounding `text-*` color via
 *  `fill="currentColor"`. This lets brand icons adapt to dark themes without
 *  a separate asset per theme. */
function BrandIcon({
  markup,
  label,
  className,
}: {
  markup: string;
  label: string;
  className?: string;
}) {
  return (
    <span
      role="img"
      aria-label={label}
      className={cn(
        "inline-flex h-3.5 w-3.5 shrink-0 items-center justify-center [&_svg]:h-full [&_svg]:w-full",
        className,
      )}
      // The SVG markup is author-controlled and shipped as a local asset, so
      // it is safe to inject.
      dangerouslySetInnerHTML={{ __html: markup }}
    />
  );
}

/* -------------------------------------------------------------------------- */
/* Segmented control                                                          */
/* -------------------------------------------------------------------------- */

function SegmentedControl<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
}: {
  value: T;
  onChange: (next: T) => void;
  options: Array<{ value: T; label: React.ReactNode; icon?: LucideIcon }>;
  ariaLabel: string;
}) {
  return (
    <div
      role="tablist"
      aria-label={ariaLabel}
      className="inline-flex flex-wrap gap-0.5 rounded-md border border-border-strong bg-surface-muted p-0.5"
    >
      {options.map((opt) => {
        const active = opt.value === value;
        const Icon = opt.icon;
        return (
          <button
            key={opt.value}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(opt.value)}
            className={cn(
              "inline-flex items-center gap-1.5 rounded-xs px-2.5 py-1 text-xs font-medium",
              "transition-colors duration-[var(--duration-fast)]",
              "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
              active
                ? "bg-surface text-text shadow-xs"
                : "text-text-muted hover:text-text",
            )}
          >
            {Icon ? <Icon size={12} aria-hidden /> : null}
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Code block with sticky header + line gutters                               */
/* -------------------------------------------------------------------------- */

function CodeBlock({
  fileName,
  caption,
  code,
  maxHeight,
  onCopy,
  copyLabel,
  copiedLabel,
  copied,
}: {
  fileName: string;
  caption?: string;
  code: string;
  /** Optional cap on the rendered code area in pixels. */
  maxHeight?: number;
  onCopy: () => void;
  copyLabel: string;
  copiedLabel: string;
  copied: boolean;
}) {
  const lineCount = code.split("\n").length;
  const lineNumberWidth = String(lineCount).length;
  const maxHeightStyle = maxHeight
    ? { maxHeight: `${maxHeight}px` }
    : undefined;

  return (
    <div className="flex h-full min-h-0 flex-col overflow-hidden rounded-md border border-border-strong bg-surface shadow-xs">
      <div className="flex shrink-0 items-center justify-between gap-2 border-b border-border-strong bg-surface-muted px-3 py-1.5">
        <div className="flex min-w-0 items-center gap-2">
          <span
            className="inline-flex items-center gap-1.5 font-mono text-[11px] text-text-muted"
            aria-label="file"
          >
            <span
              className="inline-block h-2 w-2 rounded-full bg-success"
              aria-hidden
            />
            {fileName}
          </span>
          {caption ? (
            <span className="truncate font-mono text-[10px] uppercase tracking-wide text-text-subtle">
              {caption}
            </span>
          ) : null}
        </div>
        <Button
          size="sm"
          variant="ghost"
          icon={copied ? <Check size={12} /> : <Copy size={12} />}
          onClick={onCopy}
          aria-label={copied ? copiedLabel : copyLabel}
        >
          {copied ? copiedLabel : copyLabel}
        </Button>
      </div>
      <div className="relative min-h-0 flex-1 overflow-hidden">
        <pre
          className="h-full overflow-auto bg-bg/60 py-3 font-mono text-[12.5px] leading-[1.55] text-text"
          style={maxHeightStyle}
          tabIndex={0}
        >
          <code aria-label={fileName} className="block">
            {code.split("\n").map((line, i) => (
              <div
                key={i}
                className="flex whitespace-pre px-0 hover:bg-surface-muted/60"
              >
                <span
                  className="sticky left-0 inline-block select-none border-r border-border/60 bg-bg/60 px-3 text-right text-text-subtle tabular-nums"
                  style={{ minWidth: `calc(${lineNumberWidth}ch + 1.5rem)` }}
                  aria-hidden
                >
                  {i + 1}
                </span>
                <span className="pl-3 pr-4">{line || "\u00A0"}</span>
              </div>
            ))}
          </code>
        </pre>
      </div>
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Page                                                                       */
/* -------------------------------------------------------------------------- */

export default function IntegrationGuide() {
  const { t } = useTranslation();
  const toast = useToast();

  const [baseUrl, setBaseUrl] = useState<string>(() =>
    readStoredString(STORAGE.baseUrl, DEFAULT_API_BASE),
  );
  const [model, setModel] = useState<string>(() =>
    readStoredString(STORAGE.model, DEFAULT_VIRTUAL_MODEL),
  );
  // API key is held in memory only — never persisted to localStorage.
  const [apiKey, setApiKey] = useState<string>("");
  const [stream, setStream] = useState<boolean>(() =>
    readStoredBoolean(STORAGE.stream, true),
  );
  const [activeProtocol, setActiveProtocol] = useState<ProtocolId>(() =>
    readStoredString(STORAGE.protocol, "openai") as ProtocolId,
  );
  const [activeLanguage, setActiveLanguage] = useState<LanguageId>(() =>
    readStoredString(STORAGE.language, "python") as LanguageId,
  );
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    window.localStorage.setItem(STORAGE.baseUrl, baseUrl);
  }, [baseUrl]);
  useEffect(() => {
    window.localStorage.setItem(STORAGE.model, model);
  }, [model]);
  useEffect(() => {
    window.localStorage.setItem(STORAGE.stream, stream ? "1" : "0");
  }, [stream]);
  useEffect(() => {
    window.localStorage.setItem(STORAGE.protocol, activeProtocol);
  }, [activeProtocol]);
  useEffect(() => {
    window.localStorage.setItem(STORAGE.language, activeLanguage);
  }, [activeLanguage]);

  const sample = useMemo(
    () => buildSample(activeLanguage, activeProtocol, baseUrl, model, apiKey, stream),
    [activeLanguage, activeProtocol, baseUrl, model, apiKey, stream],
  );

  const activeProtocolSpec = PROTOCOLS.find((p) => p.id === activeProtocol)!;
  const activeLanguageSpec = LANGUAGES.find((l) => l.id === activeLanguage)!;

  const isValidBase = /^https?:\/\/[^\s]+$/i.test(baseUrl.trim());
  const isValidModel = model.trim().length > 0;

  async function copySample() {
    try {
      await navigator.clipboard.writeText(sample);
      setCopied(true);
      toast.success(t("integration.sampleCopied"));
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      toast.error(t("integration.copyFailed"));
    }
  }

  return (
    <TooltipProvider delayDuration={200}>
      <div className="space-y-6">
        <PageHeader title={t("integration.title")} description={t("integration.intro")} />

        {/* ---------------------------------------------------------------- */}
        {/* Overview                                                         */}
        {/* ---------------------------------------------------------------- */}
        <Card>
          <CardHeader
            title={
              <div className="flex items-center gap-2">
                <Server size={16} className="text-primary" aria-hidden />
                {t("integration.overviewTitle")}
              </div>
            }
            description={t("integration.overviewDesc")}
          />
          <CardBody className="space-y-5">
            {/* Health check row */}
            <div className="flex flex-wrap items-center gap-3 rounded-md border border-border bg-surface-muted px-3 py-2">
              <div className="flex min-w-0 items-center gap-2">
                <Activity size={14} className="text-success" aria-hidden />
                <span className="text-xs font-medium uppercase tracking-wide text-text-subtle">
                  {t("integration.healthCheck")}
                </span>
                <code className="truncate font-mono text-xs text-text">
                  <span className="text-text-subtle">GET </span>
                  {trimTrailingSlash(baseUrl || DEFAULT_API_BASE)}
                  <span className="text-text-subtle">/healthz</span>
                </code>
              </div>
              <Badge tone="success">{t("integration.healthCheckOk")}</Badge>
            </div>

            {/* Ingress endpoints table */}
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <h3 className="text-xs font-medium uppercase tracking-wide text-text-subtle">
                  {t("integration.ingress")}
                </h3>
                <span className="text-[11px] text-text-subtle">
                  {t("integration.ingressHelp")}
                </span>
              </div>
              <ul
                className="divide-y divide-border overflow-hidden rounded-md border border-border"
                aria-label={t("integration.ingress")}
              >
                {PROTOCOLS.map((p) => {
                  return (
                    <li
                      key={p.id}
                      className="flex flex-wrap items-center gap-3 bg-surface px-3 py-2.5 sm:flex-nowrap"
                    >
                      <span className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-primary-soft text-primary">
                        <BrandIcon
                          markup={p.brand}
                          label={p.brandLabel}
                          className="h-3.5 w-3.5"
                        />
                      </span>
                      <div className="min-w-0 flex-1">
                        <div className="truncate text-sm font-medium text-text">
                          {t(p.labelKey)}
                        </div>
                        <div className="truncate text-xs text-text-muted">
                          {t(p.descriptionKey)}
                        </div>
                      </div>
                      <code className="ml-auto inline-flex items-center gap-1.5 rounded-sm bg-surface-muted px-2 py-1 font-mono text-[11px] text-text">
                        <span className="rounded-xs bg-primary-soft px-1 py-px text-[10px] font-semibold uppercase tracking-wide text-primary">
                          POST
                        </span>
                        <span>
                          {p.pathTemplate === "/v1beta/models/{model}:generateContent"
                            ? `/v1beta/models/{${model || "model"}}:generateContent`
                            : p.path}
                        </span>
                      </code>
                    </li>
                  );
                })}
              </ul>
            </div>
          </CardBody>
        </Card>

        {/* ---------------------------------------------------------------- */}
        {/* Quick start                                                       */}
        {/* ---------------------------------------------------------------- */}
        <Card>
          <CardHeader
            title={
              <div className="flex items-center gap-2">
                <Code2 size={16} className="text-primary" aria-hidden />
                {t("integration.quickstartTitle")}
              </div>
            }
            description={t("integration.quickstartDesc")}
          />
          <CardBody className="p-0">
            <div className="grid items-stretch gap-0 md:grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)] md:divide-x md:divide-border">
              {/* ----- Configuration column ----- */}
              <div className="flex flex-col gap-5 p-5">
                <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wide text-text-subtle">
                  <KeyRound size={12} aria-hidden />
                  {t("integration.configTitle")}
                </div>

                <Field label={t("integration.fieldBaseUrl")}>
                  <Input
                    value={baseUrl}
                    onChange={(e) => setBaseUrl(e.target.value)}
                    placeholder={DEFAULT_API_BASE}
                    spellCheck={false}
                    autoCapitalize="off"
                    autoCorrect="off"
                    aria-invalid={!isValidBase}
                  />
                </Field>

                <Field label={t("integration.protocolsLabel")}>
                  <Select
                    ariaLabel={t("integration.protocolsLabel")}
                    value={activeProtocol}
                    onValueChange={(v) => setActiveProtocol(v as ProtocolId)}
                    options={PROTOCOLS.map((p) => ({
                      value: p.id,
                      label: (
                        <span className="flex min-w-0 items-center gap-2">
                          <BrandIcon
                            markup={p.brand}
                            label={p.brandLabel}
                            className="h-3 w-3"
                          />
                          <span className="truncate">{t(p.labelKey)}</span>
                          <span className="ml-auto truncate font-mono text-[10px] text-text-subtle">
                            {p.pathTemplate === "/v1beta/models/{model}:generateContent"
                              ? `/v1beta/models/{…}`
                              : p.path}
                          </span>
                        </span>
                      ),
                    }))}
                  />
                </Field>

                <Field label={t("integration.fieldModel")}>
                  <Input
                    value={model}
                    onChange={(e) => setModel(e.target.value)}
                    placeholder={DEFAULT_VIRTUAL_MODEL}
                    spellCheck={false}
                    autoCapitalize="off"
                    autoCorrect="off"
                    aria-invalid={!isValidModel}
                  />
                </Field>

                <Field
                  label={t("integration.fieldApiKey")}
                  hint={t("integration.apiKeyHint")}
                >
                  <Input
                    type="password"
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                    placeholder={t("integration.fieldApiKeyPlaceholder")}
                    autoComplete="off"
                  />
                </Field>

                {/* Streaming switch in its own bordered row */}
                <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-surface-muted px-3 py-2.5">
                  <div className="min-w-0 flex-1">
                    <div className="text-sm font-medium text-text">
                      {t("integration.streamTitle")}
                    </div>
                    <p className="mt-0.5 text-xs text-text-muted">
                      {t("integration.streamHelp")}
                    </p>
                  </div>
                  <Switch
                    checked={stream}
                    onCheckedChange={setStream}
                    aria-label={t("integration.streamTitle")}
                  />
                </div>

                {/* Auth header preview */}
                <div className="rounded-md border border-border bg-bg/40 p-3">
                  <div className="mb-2 text-[11px] font-medium uppercase tracking-wide text-text-subtle">
                    {t("integration.authHeaderPreview")}
                  </div>
                  <code className="block break-all font-mono text-[12px] leading-relaxed text-text">
                    {activeProtocolSpec.auth === "x-api-key"
                      ? t("integration.authAnthropic")
                      : t("integration.authBearer")}
                  </code>
                </div>
              </div>

              {/* ----- Live preview column ----- */}
              <div className="flex min-h-0 flex-col gap-4 bg-bg/30 p-5">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wide text-text-subtle">
                    <TerminalSquare size={12} aria-hidden />
                    {t("integration.previewTitle")}
                  </div>
                </div>

                {/* Language segmented control */}
                <SegmentedControl<LanguageId>
                  ariaLabel={t("integration.languagesLabel")}
                  value={activeLanguage}
                  onChange={setActiveLanguage}
                  options={LANGUAGES.map((l) => ({
                    value: l.id,
                    label: l.brand ? (
                      <span className="inline-flex items-center gap-1.5">
                        <BrandIcon
                          markup={l.brand}
                          label={l.brandLabel ?? l.label}
                          className="h-3 w-3"
                        />
                        {l.label}
                      </span>
                    ) : (
                      l.label
                    ),
                    icon: l.brand ? undefined : l.icon,
                  }))}
                />

                <div className="min-h-0 flex-1">
                  <CodeBlock
                    fileName={activeLanguageSpec.fileName}
                    caption={t(activeProtocolSpec.pathKey, {
                      path: activeProtocolSpec.pathTemplate,
                    })}
                    code={sample}
                    maxHeight={415}
                    onCopy={copySample}
                    copyLabel={t("integration.copySample")}
                    copiedLabel={t("common.copied")}
                    copied={copied}
                  />
                </div>

                {!isValidBase ? (
                  <Alert tone="warning">{t("integration.tip.model")}</Alert>
                ) : !isValidModel ? (
                  <Alert tone="warning">{t("integration.tip.model")}</Alert>
                ) : null}
              </div>
            </div>
          </CardBody>
        </Card>

        {/* ---------------------------------------------------------------- */}
        {/* Auth & Troubleshooting                                            */}
        {/* ---------------------------------------------------------------- */}
        <Card>
          <CardHeader
            title={
              <div className="flex items-center gap-2">
                <ShieldAlert size={16} className="text-primary" aria-hidden />
                {t("integration.authTitle")} · {t("integration.troubleshootTitle")}
              </div>
            }
            description={
              <span>
                {t("integration.authDesc")} {t("integration.troubleshootDesc")}
              </span>
            }
          />
          <CardBody>
            <div className="grid gap-3 sm:grid-cols-2">
              <TipCard
                status="401"
                tone="info"
                title={t("integration.tip.auth")}
                action={{ label: t("nav.apiKeys"), to: "/api-keys" }}
              />
              <TipCard
                status="404"
                tone="warning"
                title={t("integration.tip.model")}
                action={{ label: t("nav.routes"), to: "/routes" }}
              />
              <TipCard
                status="429"
                tone="danger"
                title={t("integration.tip.quota")}
                action={{ label: t("nav.apiKeys"), to: "/api-keys" }}
              />
              <TipCard
                status="SSE"
                tone="primary"
                title={t("integration.tip.stream")}
              />
            </div>
          </CardBody>
        </Card>
      </div>
    </TooltipProvider>
  );
}

/* -------------------------------------------------------------------------- */
/* Tip card                                                                   */
/* -------------------------------------------------------------------------- */

interface TipCardProps {
  status: string;
  tone: "info" | "warning" | "danger" | "primary";
  title: string;
  action?: { label: string; to: string };
}

function TipCard({ status, tone, title, action }: TipCardProps) {
  const Icon =
    tone === "warning" ? AlertTriangle : tone === "danger" ? ShieldAlert : Activity;
  return (
    <div className="flex flex-col gap-2 rounded-md border border-border bg-surface p-3">
      <div className="flex items-center gap-2">
        <Badge tone={tone}>{status}</Badge>
        <Icon size={12} className="text-text-subtle" aria-hidden />
      </div>
      <p className="text-sm text-text-muted">{title}</p>
      {action ? (
        <Link
          to={action.to}
          className="mt-auto inline-flex items-center gap-1 self-start rounded-xs text-xs font-medium text-primary transition-colors duration-[var(--duration-fast)] hover:text-primary-strong focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          {action.label}
          <ArrowUpRight size={12} aria-hidden />
        </Link>
      ) : null}
    </div>
  );
}
