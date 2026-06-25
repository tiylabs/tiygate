/**
 * Parse `code` and `state` from a pasted callback input.
 *
 * The input can be:
 * - A full redirect URL containing `code` (and optionally `state`) query
 *   parameters, e.g. `http://127.0.0.1:56121/callback?code=…&state=…`
 * - A bare authorization code, e.g. `abc123` — the entire trimmed content
 *   is treated as the code.
 *
 * When `state` cannot be extracted from a URL, the `fallbackState`
 * returned by `/oauth/start` is used instead. Returns `null` if no code
 * can be determined.
 */
export function parseCallbackUrl(
  raw: string,
  fallbackState?: string,
): { code: string; state: string } | null {
  const trimmed = raw.trim();
  if (!trimmed) return null;

  try {
    const url = new URL(trimmed);
    const code = url.searchParams.get("code");
    const state = url.searchParams.get("state") ?? fallbackState;
    if (!code || !state) return null;
    return { code, state };
  } catch {
    // Not a URL — treat the entire content as a bare authorization code.
    if (!fallbackState) return null;
    return { code: trimmed, state: fallbackState };
  }
}
