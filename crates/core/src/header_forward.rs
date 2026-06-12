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

use std::collections::HashSet;

/// Request-direction default denylist (client → provider).
///
/// These client headers are **not** forwarded upstream:
/// - credentials the client uses against the *gateway* (must never
///   leak to the provider, and the gateway injects its own):
///   `authorization`, `proxy-authorization`, `x-api-key`,
///   `anthropic-version`, `cookie`
/// - headers the gateway recomputes because it rewrites the body or
///   controls the connection: `host`, `content-length`,
///   `content-type`, `content-encoding`, `accept-encoding`, `expect`
/// - hop-by-hop headers (RFC 7230 §6.1) that proxies must not forward:
///   `connection`, `keep-alive`, `proxy-connection`, `te`, `trailer`,
///   `transfer-encoding`, `upgrade`
/// - trace context the gateway re-mints and re-injects:
///   `traceparent`, `tracestate`
const DEFAULT_REQUEST_DENY: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
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
    "content-length",
    "content-encoding",
    "content-type",
    "date",
];

/// A bidirectional header forwarding policy. Header names are compared
/// case-insensitively (stored lowercase).
#[derive(Debug, Clone)]
pub struct HeaderForwardPolicy {
    request_deny: HashSet<String>,
    response_deny: HashSet<String>,
}

impl HeaderForwardPolicy {
    /// Build a policy seeded with the two hardcoded default denylists.
    pub fn with_defaults() -> Self {
        Self {
            request_deny: DEFAULT_REQUEST_DENY.iter().map(|s| s.to_string()).collect(),
            response_deny: DEFAULT_RESPONSE_DENY.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Build an empty policy (forwards everything). Mainly for tests.
    pub fn empty() -> Self {
        Self {
            request_deny: HashSet::new(),
            response_deny: HashSet::new(),
        }
    }

    /// Append extra header names to the request-direction denylist.
    pub fn with_request_deny_extra<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let key = n.as_ref().trim().to_lowercase();
            if !key.is_empty() {
                self.request_deny.insert(key);
            }
        }
        self
    }

    /// Append extra header names to the response-direction denylist.
    pub fn with_response_deny_extra<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in names {
            let key = n.as_ref().trim().to_lowercase();
            if !key.is_empty() {
                self.response_deny.insert(key);
            }
        }
        self
    }

    /// Whether a client request header should be forwarded upstream.
    pub fn should_forward_request(&self, name: &str) -> bool {
        !self.request_deny.contains(&name.to_lowercase())
    }

    /// Whether an upstream response header should be forwarded to the
    /// client.
    pub fn should_forward_response(&self, name: &str) -> bool {
        !self.response_deny.contains(&name.to_lowercase())
    }
}

impl Default for HeaderForwardPolicy {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Trace.
        assert!(!p.should_forward_request("traceparent"));
        // Normal custom header is forwarded.
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
        assert!(!p.should_forward_response("date"));
        // retry-after / rate limit flow through.
        assert!(p.should_forward_response("retry-after"));
        assert!(p.should_forward_response("x-ratelimit-remaining"));
        // Provider diagnostics flow through.
        assert!(p.should_forward_response("x-request-id"));
    }

    #[test]
    fn extra_denies_are_applied_case_insensitively() {
        let p = HeaderForwardPolicy::with_defaults()
            .with_request_deny_extra(["X-Stainless-Lang", "  ", "x-internal"])
            .with_response_deny_extra(["X-Server-Secret"]);
        assert!(!p.should_forward_request("x-stainless-lang"));
        assert!(!p.should_forward_request("x-internal"));
        assert!(!p.should_forward_response("x-server-secret"));
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
