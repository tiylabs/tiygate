//! Header forwarding policy — denylist-based passthrough.
//!
//! The gateway forwards client request headers to the upstream
//! provider (C→G→P) and upstream response headers back to the client
//! (P→G→C) **by default**, blocking only the headers named in two
//! hardcoded denylists (plus any operator-supplied extras). This
//! maximizes debuggability while protecting credentials, connection
//! semantics, and gateway-controlled headers.
//!
//! This policy is intentionally separate from the [`crate::redaction`]
//! `Redactor`: redaction decides what is *masked in the logs*, while
//! this policy decides what is *actually forwarded on the wire*.
//!
//! ## Pattern syntax
//!
//! Each deny entry is either a **literal** header name (exact match,
//! case-insensitive) or a **glob pattern** using `*` as a wildcard:
//!
//! | Pattern           | Meaning                                    |
//! |-------------------|--------------------------------------------|
//! | `authorization`   | Blocks exactly `authorization`             |
//! | `*request-id`     | Blocks any header ending in `request-id`   |
//! | `x-internal-*`    | Blocks any header starting with `x-internal-` |
//! | `*debug*`         | Blocks any header containing `debug`       |
//!
//! Patterns without `*` are stored in a `HashSet` for O(1) lookup;
//! patterns with `*` are matched via linear scan (the list is short).

use std::collections::HashSet;

/// Request-direction default denylist (client → provider).
///
/// These client headers are **not** forwarded upstream:
/// - credentials the client uses against the *gateway* (must never
///   leak to the provider, and the gateway injects its own):
///   `authorization`, `proxy-authorization`, `x-api-key`,
///   `x-goog-api-key`, `anthropic-version`, `cookie`
/// - headers the gateway recomputes because it rewrites the body or
///   controls the connection: `host`, `content-length`,
///   `content-type`, `content-encoding`, `accept-encoding`, `expect`
/// - hop-by-hop headers (RFC 7230 §6.1) that proxies must not forward:
///   `connection`, `keep-alive`, `proxy-connection`, `te`, `trailer`,
///   `transfer-encoding`, `upgrade`, `sec-websocket-*`
/// - trace context the gateway re-mints and re-injects:
///   `traceparent`, `tracestate`
const DEFAULT_REQUEST_DENY: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-goog-api-key",
    "anthropic-version",
    "cookie",
    "host",
    "content-length",
    "content-type",
    "content-encoding",
    "accept-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "sec-websocket-*",
    "traceparent",
    "tracestate",
    "expect",
];

/// Response-direction default denylist (provider → client).
///
/// These upstream response headers are **not** forwarded to the client:
/// - hop-by-hop headers that would break the downstream connection:
///   `connection`, `keep-alive`, `proxy-connection`, `te`, `trailer`,
///   `transfer-encoding`, `upgrade`
/// - length/encoding headers that no longer match after the gateway
///   re-serializes the body or reqwest auto-decompresses:
///   `content-length`, `content-encoding`
/// - headers the response framework sets itself: `content-type`
///   (chosen by `Json`/`Sse`), `date`
/// - provider request-id headers — the gateway injects its own
///   `x-request-id` after forwarding: `*request-id`, `*requestid` (glob)
/// - origin server identity / infrastructure headers that leak
///   upstream topology: `server`, `via`
/// - reporting / debugging / CDN headers the client should not see:
///   `nel`, `cf-*` (Cloudflare), `eo-*` (EdgeOne), `report-to`
/// - CORS headers the gateway does not proxy:
///   `access-control-*` (glob)
/// - security headers that are domain-specific to the upstream
///   provider (the gateway applies its own if needed):
///   `strict-transport-security`
/// - HTTP negotiation headers the gateway may invalidate by
///   modifying the response: `vary`
/// - upstream trace headers (the gateway mints its own trace
///   identifiers): `*trace*` (glob)
///
/// Note: `retry-after` and `x-ratelimit-*` are intentionally **not**
/// in the denylist, so they flow through the generic forwarder.
const DEFAULT_RESPONSE_DENY: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "sec-websocket-*",
    "content-length",
    "content-encoding",
    "content-type",
    "date",
    "*request-id",
    "*requestid",
    "server",
    "via",
    "nel",
    "cf-*",
    "eo-*",
    "report-to",
    "access-control-*",
    "strict-transport-security",
    "*trace*",
    "vary",
];

/// A bidirectional header forwarding policy. Header names are compared
/// case-insensitively (stored lowercase).
///
/// Each direction has two deny collections: an exact-match `HashSet`
/// for O(1) literal lookups, and a `Vec` of glob patterns (entries
/// containing `*`) matched via linear scan.
#[derive(Debug, Clone)]
pub struct HeaderForwardPolicy {
    request_deny_exact: HashSet<String>,
    request_deny_globs: Vec<String>,
    response_deny_exact: HashSet<String>,
    response_deny_globs: Vec<String>,
}

impl HeaderForwardPolicy {
    /// Build a policy seeded with the hardcoded default denylists.
    pub fn with_defaults() -> Self {
        let (req_exact, req_globs) = Self::partition_entries(DEFAULT_REQUEST_DENY);
        let (resp_exact, resp_globs) = Self::partition_entries(DEFAULT_RESPONSE_DENY);
        Self {
            request_deny_exact: req_exact,
            request_deny_globs: req_globs,
            response_deny_exact: resp_exact,
            response_deny_globs: resp_globs,
        }
    }

    /// Build an empty policy (forwards everything). Mainly for tests.
    pub fn empty() -> Self {
        Self {
            request_deny_exact: HashSet::new(),
            request_deny_globs: Vec::new(),
            response_deny_exact: HashSet::new(),
            response_deny_globs: Vec::new(),
        }
    }

    /// Append extra deny entries to the request-direction denylist.
    /// Entries may be literal names or glob patterns (containing `*`).
    pub fn with_request_deny_extra<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let key = n.as_ref().trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            if key.contains('*') {
                self.request_deny_globs.push(key);
            } else {
                self.request_deny_exact.insert(key);
            }
        }
        self
    }

    /// Append extra deny entries to the response-direction denylist.
    /// Entries may be literal names or glob patterns (containing `*`).
    pub fn with_response_deny_extra<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let key = n.as_ref().trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            if key.contains('*') {
                self.response_deny_globs.push(key);
            } else {
                self.response_deny_exact.insert(key);
            }
        }
        self
    }

    /// Whether a client request header should be forwarded upstream.
    pub fn should_forward_request(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        if self.request_deny_exact.contains(&lower) {
            return false;
        }
        if self
            .request_deny_globs
            .iter()
            .any(|g| glob_match(g, &lower))
        {
            return false;
        }
        true
    }

    /// Whether an upstream response header should be forwarded to the
    /// client.
    pub fn should_forward_response(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        if self.response_deny_exact.contains(&lower) {
            return false;
        }
        if self
            .response_deny_globs
            .iter()
            .any(|g| glob_match(g, &lower))
        {
            return false;
        }
        true
    }

    // ── internal helpers ────────────────────────────────────────────

    /// Split a list of deny entries into exact names and glob patterns.
    fn partition_entries(entries: &[&str]) -> (HashSet<String>, Vec<String>) {
        let mut exact = HashSet::new();
        let mut globs = Vec::new();
        for &e in entries {
            let key = e.to_lowercase();
            if key.contains('*') {
                globs.push(key);
            } else {
                exact.insert(key);
            }
        }
        (exact, globs)
    }
}

impl Default for HeaderForwardPolicy {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Minimal glob matcher for header-name patterns. Supports `*` as a
/// wildcard that matches zero or more characters. Multiple `*` are
/// supported. Both `pattern` and `value` are expected to be lowercase.
fn glob_match(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();

    // Fast path: no wildcard (should not happen — caller checks, but
    // be safe).
    if parts.len() == 1 {
        return pattern == value;
    }

    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            // Leading, trailing, or consecutive `*` — matches anything.
            continue;
        }
        if let Some(found) = value[pos..].find(part) {
            // First segment must anchor at the start if the pattern
            // does not begin with `*`.
            if i == 0 && found != 0 {
                return false;
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // If the pattern does not end with `*`, the value must end exactly
    // where the last segment ended.
    if !pattern.ends_with('*') {
        return pos == value.len();
    }

    true
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    // ── glob_match unit tests ───────────────────────────────────────

    #[test]
    fn glob_match_suffix() {
        assert!(glob_match("*request-id", "x-request-id"));
        assert!(glob_match("*request-id", "x-oneapi-request-id"));
        assert!(glob_match("*request-id", "request-id"));
        assert!(glob_match("*requestid", "x-zenmux-requestid"));
        assert!(glob_match("*requestid", "requestid"));
        assert!(!glob_match("*request-id", "x-request-id-extra"));
        assert!(!glob_match("*request-id", "x-debug-id"));
    }

    #[test]
    fn glob_match_prefix() {
        assert!(glob_match("x-internal-*", "x-internal-foo"));
        assert!(glob_match("x-internal-*", "x-internal-"));
        assert!(!glob_match("x-internal-*", "x-external-foo"));
    }

    #[test]
    fn glob_match_contains() {
        assert!(glob_match("*debug*", "x-debug-id"));
        assert!(glob_match("*debug*", "debug"));
        assert!(!glob_match("*debug*", "x-trace-id"));
    }

    #[test]
    fn glob_match_exact_no_wildcard() {
        assert!(glob_match("host", "host"));
        assert!(!glob_match("host", "hostname"));
    }

    // ── policy tests ────────────────────────────────────────────────

    #[test]
    fn default_request_denylist_blocks_credentials_and_hop_by_hop() {
        let p = HeaderForwardPolicy::with_defaults();
        // Credentials / gateway-injected.
        assert!(!p.should_forward_request("authorization"));
        assert!(!p.should_forward_request("Authorization")); // case-insensitive
        assert!(!p.should_forward_request("x-api-key"));
        assert!(!p.should_forward_request("anthropic-version"));
        // Gateway-controlled.
        assert!(!p.should_forward_request("host"));
        assert!(!p.should_forward_request("content-length"));
        assert!(!p.should_forward_request("content-type"));
        // Hop-by-hop.
        assert!(!p.should_forward_request("connection"));
        assert!(!p.should_forward_request("transfer-encoding"));
        assert!(!p.should_forward_request("sec-websocket-key"));
        // Trace.
        assert!(!p.should_forward_request("traceparent"));
        // Normal custom header is forwarded.
        assert!(p.should_forward_request("x-debug-id"));
        assert!(p.should_forward_request("x-correlation-id"));
    }

    #[test]
    fn default_request_denylist_forwards_client_request_ids() {
        let p = HeaderForwardPolicy::with_defaults();
        assert!(p.should_forward_request("x-request-id"));
        assert!(p.should_forward_request("x-oneapi-request-id"));
        assert!(p.should_forward_request("X-Stainless-Request-Id"));
        assert!(p.should_forward_request("request-id"));
        assert!(p.should_forward_request("x-debug-id"));
        assert!(p.should_forward_request("x-correlation-id"));
    }

    #[test]
    fn default_response_denylist_blocks_hop_by_hop_and_framework_headers() {
        let p = HeaderForwardPolicy::with_defaults();
        assert!(!p.should_forward_response("transfer-encoding"));
        assert!(!p.should_forward_response("Transfer-Encoding"));
        assert!(!p.should_forward_response("content-length"));
        assert!(!p.should_forward_response("content-type"));
        assert!(!p.should_forward_response("connection"));
        assert!(!p.should_forward_response("sec-websocket-accept"));
        assert!(!p.should_forward_response("date"));
        // Infrastructure / topology.
        assert!(!p.should_forward_response("server"));
        assert!(!p.should_forward_response("via"));
        // Reporting / CDN.
        assert!(!p.should_forward_response("nel"));
        assert!(!p.should_forward_response("cf-ray"));
        assert!(!p.should_forward_response("cf-cache-status"));
        assert!(!p.should_forward_response("eo-log-uuid"));
        assert!(!p.should_forward_response("eo-cache-status"));
        assert!(!p.should_forward_response("report-to"));
        // CORS (glob).
        assert!(!p.should_forward_response("access-control-allow-origin"));
        assert!(!p.should_forward_response("access-control-allow-headers"));
        assert!(!p.should_forward_response("access-control-expose-headers"));
        // Security / negotiation / trace.
        assert!(!p.should_forward_response("strict-transport-security"));
        assert!(!p.should_forward_response("Strict-Transport-Security"));
        assert!(!p.should_forward_response("vary"));
        assert!(!p.should_forward_response("traceresponse"));
        assert!(!p.should_forward_response("x-trace-id"));
        // retry-after / rate limit flow through.
        assert!(p.should_forward_response("retry-after"));
        assert!(p.should_forward_response("x-ratelimit-remaining"));
        // Provider request-id headers are blocked by the glob rule;
        // the gateway injects its own `x-request-id` after forwarding.
        assert!(!p.should_forward_response("x-request-id"));
        assert!(!p.should_forward_response("x-oneapi-request-id"));
        assert!(!p.should_forward_response("x-stainless-request-id"));
        assert!(!p.should_forward_response("x-zenmux-requestid"));
        assert!(!p.should_forward_response("requestid"));
    }

    #[test]
    fn extra_denies_support_globs_and_literals() {
        let p = HeaderForwardPolicy::with_defaults()
            .with_request_deny_extra(["X-Stainless-Lang", "  ", "x-internal-*"])
            .with_response_deny_extra(["X-Server-Secret", "*trace*"]);
        // Exact extras.
        assert!(!p.should_forward_request("x-stainless-lang"));
        assert!(!p.should_forward_response("x-server-secret"));
        // Glob extras.
        assert!(!p.should_forward_request("x-internal-foo"));
        assert!(!p.should_forward_request("x-internal-bar"));
        assert!(!p.should_forward_response("x-trace-id"));
        assert!(!p.should_forward_response("x-my-trace-flag"));
        // Empty/whitespace entries are ignored, others still forward.
        assert!(p.should_forward_request("x-debug-id"));
    }

    #[test]
    fn empty_policy_forwards_everything() {
        let p = HeaderForwardPolicy::empty();
        assert!(p.should_forward_request("authorization"));
        assert!(p.should_forward_response("transfer-encoding"));
    }
}
