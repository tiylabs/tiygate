import { getToken, clearToken } from "@/auth/token";

/**
 * Resolve the API base URL. In a browser (non-Tauri) environment the
 * SPA is served by the same origin as the API, so a relative path
 * works. In a Tauri webview the SPA is served from `tauri://localhost`
 * while the API lives on a remote or local server, so we need an
 * absolute URL. The active instance info is fetched from the Tauri
 * backend and cached.
 */
let apiBaseUrl = "/admin/v1";
let portResolved = false;

/**
 * The instance key used to scope token storage. `"local"` for the
 * sidecar, or a remote instance id. Set by `setActiveInstance` /
 * `ensureApiBase` so that `apiRequest` reads the correct per-instance
 * token from storage.
 */
let currentInstanceKey = "";

/** Generate a client mutation key so a retry after a lost response can be
 * replayed safely by the Admin API instead of applying the write twice. */
export function newIdempotencyKey(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `ui-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

async function ensureApiBase(): Promise<void> {
  if (portResolved) return;
  const tauriInternals =
    typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  if (!tauriInternals) {
    portResolved = true;
    return;
  }
  try {
    // Fetch the active instance from the Rust backend. For local
    // instances this returns the 127.0.0.1 origin + port; for remote
    // instances it returns the user-configured URL.
    const mod = await import("@tauri-apps/api/core");
    const active = await mod.invoke<{
      kind: string;
      id?: string;
      url?: string;
    }>("get_active_instance");
    if (active?.url) {
      apiBaseUrl = `${active.url.replace(/\/+$/, "")}/admin/v1`;
    }
    currentInstanceKey = active?.id ?? "local";
  } catch {
    // Degrade to relative path — will fail with connection errors,
    // but that's expected if the sidecar isn't running.
  }
  portResolved = true;
}

/**
 * Force-set the API base URL after an instance switch, bypassing the
 * ensureApiBase lookup. The caller passes the full base (origin for
 * local, URL for remote); this function appends `/admin/v1`.
 * Also sets the instance key for per-instance token scoping.
 */
export function setActiveInstance(baseUrl: string, instanceKey: string): void {
  apiBaseUrl = `${baseUrl.replace(/\/+$/, "")}/admin/v1`;
  currentInstanceKey = instanceKey;
  portResolved = true;
}

/** Reset the cached base URL (used when switching instances or when the sidecar restarts on a new port). */
export function resetApiBase(): void {
  portResolved = false;
  apiBaseUrl = "/admin/v1";
  currentInstanceKey = "";
}

/**
 * A normalized API error. The admin API uses two envelope shapes:
 *  - main handlers: `{ "error": { "message", "type", "source" } }`
 *  - oauth subroutes: `{ "error": "<message>" }`
 * Both are flattened into `message` here, with the HTTP status kept
 * so callers can branch on 401 / 503 / 404 etc.
 */
export class ApiError extends Error {
  status: number;
  type?: string;
  code?: string;

  constructor(status: number, message: string, type?: string, code?: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.type = type;
    this.code = code;
  }
}

/** Listeners notified when the API returns 401 (token rejected). */
type UnauthorizedHandler = () => void;
let onUnauthorized: UnauthorizedHandler | null = null;
export function setUnauthorizedHandler(fn: UnauthorizedHandler | null): void {
  onUnauthorized = fn;
}

async function parseError(res: Response): Promise<ApiError> {
  let message = `${res.status} ${res.statusText}`;
  let type: string | undefined;
  let code: string | undefined;
  try {
    const body = await res.json();
    const err = (body as Record<string, unknown>)?.error;
    if (typeof err === "string") {
      message = err;
    } else if (err && typeof err === "object") {
      const obj = err as Record<string, unknown>;
      if (typeof obj.message === "string") message = obj.message;
      if (typeof obj.type === "string") type = obj.type;
      if (typeof obj.code === "string") code = obj.code;
    }
  } catch {
    // Non-JSON body; keep the status line as the message.
  }
  return new ApiError(res.status, message, type, code);
}

interface RequestOptions {
  method?: string;
  body?: unknown;
  query?: Record<string, string | number | boolean | undefined | null>;
  headers?: Record<string, string>;
  /** Set to true for endpoints that may return 204 No Content. */
  allowEmpty?: boolean;
}

function buildUrl(path: string, query?: RequestOptions["query"]): string {
  const url = `${apiBaseUrl}${path}`;
  if (!query) return url;
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== null && v !== "") {
      params.set(k, String(v));
    }
  }
  const qs = params.toString();
  return qs ? `${url}?${qs}` : url;
}

export async function apiRequest<T>(
  path: string,
  opts: RequestOptions = {},
): Promise<T> {
  await ensureApiBase();
  const token = getToken(currentInstanceKey);
  const headers: Record<string, string> = {};
  if (token) headers["Authorization"] = `Bearer ${token}`;
  if (opts.body !== undefined) headers["Content-Type"] = "application/json";
  Object.assign(headers, opts.headers);

  const res = await fetch(buildUrl(path, opts.query), {
    method: opts.method ?? "GET",
    headers,
    body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
  });

  if (res.status === 401) {
    // The token was rejected. Drop it and let the app route back to
    // the login screen.
    clearToken(currentInstanceKey);
    onUnauthorized?.();
    throw await parseError(res);
  }

  if (!res.ok) {
    throw await parseError(res);
  }

  if (res.status === 204 || opts.allowEmpty) {
    // No body to parse (DELETE / disable). Return undefined cast.
    const text = await res.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  return (await res.json()) as T;
}

/**
 * Fetch server info (name + version) from the public `/admin/v1/info`
 * endpoint. This endpoint is exempt from bearer-token auth so the
 * login page can display the version before the user has a token.
 * Returns `null` on any error so callers can degrade gracefully.
 */
export async function fetchServerInfo(): Promise<{
  name: string;
  version: string;
} | null> {
  await ensureApiBase();
  try {
    const res = await fetch(buildUrl("/info"));
    if (!res.ok) return null;
    return (await res.json()) as { name: string; version: string };
  } catch {
    return null;
  }
}

/**
 * Probe the admin token by hitting a cheap protected endpoint.
 * Returns true on 2xx, false on 401, and rethrows other errors
 * (e.g. 503 when TIYGATE_ADMIN_TOKEN is not configured server-side).
 */
export async function probeToken(token: string): Promise<void> {
  await ensureApiBase();
  const res = await fetch(buildUrl("/audit", { limit: 1 }), {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (res.ok) return;
  throw await parseError(res);
}
