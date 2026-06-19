import { getToken, clearToken } from "@/auth/token";

/** Base path for the admin REST API. Same-origin with the SPA. */
const API_BASE = "/admin/v1";

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

  constructor(status: number, message: string, type?: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.type = type;
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
  try {
    const body = await res.json();
    const err = (body as Record<string, unknown>)?.error;
    if (typeof err === "string") {
      message = err;
    } else if (err && typeof err === "object") {
      const obj = err as Record<string, unknown>;
      if (typeof obj.message === "string") message = obj.message;
      if (typeof obj.type === "string") type = obj.type;
    }
  } catch {
    // Non-JSON body; keep the status line as the message.
  }
  return new ApiError(res.status, message, type);
}

interface RequestOptions {
  method?: string;
  body?: unknown;
  query?: Record<string, string | number | boolean | undefined | null>;
  /** Set to true for endpoints that may return 204 No Content. */
  allowEmpty?: boolean;
}

function buildUrl(
  path: string,
  query?: RequestOptions["query"],
): string {
  const url = `${API_BASE}${path}`;
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
  const token = getToken();
  const headers: Record<string, string> = {};
  if (token) headers["Authorization"] = `Bearer ${token}`;
  if (opts.body !== undefined) headers["Content-Type"] = "application/json";

  const res = await fetch(buildUrl(path, opts.query), {
    method: opts.method ?? "GET",
    headers,
    body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined,
  });

  if (res.status === 401) {
    // The token was rejected. Drop it and let the app route back to
    // the login screen.
    clearToken();
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
export async function fetchServerInfo(): Promise<{ name: string; version: string } | null> {
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
  const res = await fetch(buildUrl("/audit", { limit: 1 }), {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (res.ok) return;
  throw await parseError(res);
}
