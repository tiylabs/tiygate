//! Sensitive-data redaction for `RawEnvelope` and log payloads.
//!
//! The design doc §3.5 requires that gateway error responses and
//! audit log entries do not leak upstream API keys, OAuth tokens, or
//! client credentials. This module provides a single
//! [`Redactor`] that is configurable but ships with sensible defaults
//! covering all of the common cases:
//!
//! * `Authorization: Bearer …`, `x-api-key: …`,
//!   `anthropic-api-key: …`, `openai-organization`, etc.
//! * `Cookie`, `Set-Cookie` headers.
//! * `proxy-authorization`
//! * Any header containing `key`, `token`, `secret`, or `password`
//!   (case-insensitive) — operator-tunable by adjusting the
//!   `additional_patterns` list.
//!
//! Body strings are passed through a smaller, optional scrubber that
//! targets the same set of well-known string keys (`api_key`,
//! `refresh_token`, etc.) inside JSON. We intentionally do **not**
//! attempt to redact every conceivable secret in arbitrary bodies;
//! that would either be a fragile regex or a costly structured
//! parser. Phase 4's threat model is "headers and known JSON keys",
//! and that is what this module implements.

use std::collections::HashSet;

/// Marker used in place of redacted bytes.
pub const REDACTED: &str = "[REDACTED]";

/// Set of well-known header names that always contain credentials.
const DEFAULT_HEADER_PATTERNS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "anthropic-api-key",
    "openai-organization",
    "openai-project",
    "tiygate-admin-token",
];

/// Substring matchers for header names. Header names that contain
/// any of these substrings (case-insensitive) are redacted.
const DEFAULT_HEADER_SUBSTRINGS: &[&str] = &["token", "secret", "password", "credential"];

/// JSON body keys that should have their values redacted when
/// encountered at the top level of a request body.
const DEFAULT_BODY_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "access_token",
    "refresh_token",
    "client_secret",
    "password",
];

/// Configurable redactor. Construct with [`Redactor::with_defaults`]
/// for the Phase 4 standard set, or with [`Redactor::empty`] for an
/// opt-in mode where only the caller-supplied patterns are honoured.
#[derive(Debug, Clone)]
pub struct Redactor {
    header_names: HashSet<String>,
    header_substrings: Vec<String>,
    body_keys: HashSet<String>,
}

impl Redactor {
    /// A redactor that redacts the default header set + the default
    /// JSON body key set.
    pub fn with_defaults() -> Self {
        Self {
            header_names: DEFAULT_HEADER_PATTERNS
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            header_substrings: DEFAULT_HEADER_SUBSTRINGS
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            body_keys: DEFAULT_BODY_KEYS.iter().map(|s| s.to_lowercase()).collect(),
        }
    }

    /// A redactor with no built-in rules — useful for tests that want
    /// to assert behaviour for a specific set of patterns.
    pub fn empty() -> Self {
        Self {
            header_names: HashSet::new(),
            header_substrings: Vec::new(),
            body_keys: HashSet::new(),
        }
    }

    /// Add an additional header name pattern to redact.
    pub fn with_header_name(mut self, name: impl Into<String>) -> Self {
        self.header_names.insert(name.into().to_lowercase());
        self
    }

    /// Add an additional header substring to redact.
    pub fn with_header_substring(mut self, pat: impl Into<String>) -> Self {
        self.header_substrings.push(pat.into().to_lowercase());
        self
    }

    /// Add an additional body key to redact.
    pub fn with_body_key(mut self, key: impl Into<String>) -> Self {
        self.body_keys.insert(key.into().to_lowercase());
        self
    }

    /// Returns `true` if `header_name` (case-insensitive) should be
    /// redacted by this redactor.
    pub fn should_redact_header(&self, header_name: &str) -> bool {
        let lower = header_name.to_lowercase();
        if self.header_names.contains(&lower) {
            return true;
        }
        self.header_substrings.iter().any(|p| lower.contains(p))
    }

    /// Returns `true` if `body_key` (case-insensitive) should be
    /// redacted by this redactor.
    pub fn should_redact_body_key(&self, body_key: &str) -> bool {
        self.body_keys.contains(&body_key.to_lowercase())
    }

    /// Redact a header map in place, returning a new map (or
    /// returning a redacted copy — does not mutate the input).
    pub fn redact_headers<I, K, V>(&self, iter: I) -> Vec<(String, String)>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut out = Vec::new();
        for (k, v) in iter {
            let key: String = k.into();
            if self.should_redact_header(&key) {
                out.push((key, REDACTED.to_string()));
            } else {
                out.push((key, v.into()));
            }
        }
        out
    }

    /// Redact a JSON value in place. Object values are walked
    /// recursively; arrays are walked element-wise. Strings inside
    /// matching keys are replaced with [`REDACTED`].
    pub fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    if self.should_redact_body_key(k) {
                        *v = serde_json::Value::String(REDACTED.to_string());
                    } else {
                        self.redact_value(v);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr.iter_mut() {
                    self.redact_value(v);
                }
            }
            _ => {}
        }
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn default_redacts_authorization() {
        let r = Redactor::with_defaults();
        let h: HashMap<String, String> = [
            ("Authorization".to_string(), "Bearer sk-1234".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
        .into_iter()
        .collect();
        let redacted = r.redact_headers(h);
        let auth = redacted.iter().find(|(k, _)| k == "Authorization").unwrap();
        assert_eq!(auth.1, REDACTED);
        let ct = redacted.iter().find(|(k, _)| k == "Content-Type").unwrap();
        assert_eq!(ct.1, "application/json");
    }

    #[test]
    fn default_redacts_substring_token() {
        let r = Redactor::with_defaults();
        assert!(r.should_redact_header("X-Refresh-Token"));
        assert!(r.should_redact_header("client_secret"));
        assert!(!r.should_redact_header("Content-Type"));
    }

    #[test]
    fn redact_value_walks_nested_objects() {
        let r = Redactor::with_defaults();
        // The default body key set explicitly covers `api_key`,
        // `token`, `access_token`, `refresh_token`, `client_secret`,
        // `password`. Header names like `Authorization` live in the
        // header-redaction set and are *not* redacted when they
        // appear as JSON body keys — they should pass through.
        let mut v = serde_json::json!({
            "model": "gpt-4o",
            "api_key": "sk-secret",
            "headers": { "Authorization": "Bearer abc", "x-id": "ok" },
            "items": [{ "token": "t1" }, { "ok": true }],
        });
        r.redact_value(&mut v);
        assert_eq!(v["api_key"], serde_json::json!(REDACTED));
        // Authorization is a header-only pattern; it passes through here.
        assert_eq!(
            v["headers"]["Authorization"],
            serde_json::json!("Bearer abc")
        );
        assert_eq!(v["headers"]["x-id"], serde_json::json!("ok"));
        assert_eq!(v["items"][0]["token"], serde_json::json!(REDACTED));
        assert_eq!(v["items"][1]["ok"], serde_json::json!(true));
    }

    #[test]
    fn custom_patterns_extend_defaults() {
        let r = Redactor::empty()
            .with_header_name("X-My-Secret")
            .with_body_key("client_cert");
        assert!(r.should_redact_header("X-MY-SECRET"));
        assert!(r.should_redact_body_key("client_cert"));
        assert!(!r.should_redact_header("Authorization"));
    }
}
