//! W3C Trace Context utilities — extract `traceparent` / `tracestate`
//! from incoming HTTP headers and inject them into outgoing upstream
//! requests. See design doc §4.8 for the contract.
//!
//! Format reference (RFC):
//!   `traceparent: <version>-<trace-id>-<parent-id>-<trace-flags>`
//!     e.g. `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`
//!
//! We *do not* validate the version byte (always `00` today, reserved
//! for future versions) and we *do* validate the field lengths. A
//! malformed header is treated as "no trace context present" — the
//! caller falls back to generating a new root trace.

use std::sync::Arc;

use parking_lot::Mutex;
use rand::RngCore;

/// Length of a W3C trace-id in hex characters.
pub const TRACE_ID_LEN: usize = 32;
/// Length of a W3C span-id in hex characters.
pub const SPAN_ID_LEN: usize = 16;

/// A parsed W3C Trace Context value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    /// 32-char lowercase hex (16 bytes).
    pub trace_id: String,
    /// 16-char lowercase hex (8 bytes) — the caller's parent span.
    pub parent_span_id: String,
    /// W3C trace-flags (single byte, e.g. `01` for sampled).
    pub flags: u8,
    /// Optional `tracestate` header value (forwarded as-is, no
    /// semantic interpretation).
    pub tracestate: Option<String>,
}

impl TraceContext {
    /// Reconstruct the canonical `traceparent` header value.
    pub fn to_traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id, self.parent_span_id, self.flags
        )
    }

    /// Build a `TraceContext` from a raw `traceparent` string. If
    /// the input is empty or malformed, return a fresh root
    /// (`00-<new-trace-id>-<new-span-id>-01`).
    pub fn from_raw(raw: &str) -> Self {
        match extract_traceparent(raw) {
            TraceContextExtraction::Present(mut ctx) => {
                ctx.parent_span_id = new_span_id();
                ctx
            }
            _ => Self {
                trace_id: new_trace_id(),
                parent_span_id: new_span_id(),
                flags: 0x01,
                tracestate: None,
            },
        }
    }
}

/// Outcome of an extraction attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceContextExtraction {
    /// A valid `traceparent` was present.
    Present(TraceContext),
    /// Either no `traceparent` header was supplied, or it was
    /// malformed; treat this as a request without trace context.
    Absent,
}

impl TraceContextExtraction {
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }
}

/// Extract a [`TraceContext`] from a `traceparent` header value. The
/// raw header is `version-traceid-parentid-flags`; we accept `00` as
/// the only version today.
pub fn extract_traceparent(value: &str) -> TraceContextExtraction {
    let parts: Vec<&str> = value.splitn(4, '-').collect();
    if parts.len() != 4 {
        return TraceContextExtraction::Absent;
    }
    let version = parts[0];
    let trace_id = parts[1];
    let parent_id = parts[2];
    let flags_hex = parts[3];
    if version != "00" {
        return TraceContextExtraction::Absent;
    }
    if trace_id.len() != TRACE_ID_LEN || !is_lower_hex(trace_id) {
        return TraceContextExtraction::Absent;
    }
    if parent_id.len() != SPAN_ID_LEN || !is_lower_hex(parent_id) {
        return TraceContextExtraction::Absent;
    }
    let flags = match u8::from_str_radix(flags_hex, 16) {
        Ok(f) => f,
        Err(_) => return TraceContextExtraction::Absent,
    };
    TraceContextExtraction::Present(TraceContext {
        trace_id: trace_id.to_string(),
        parent_span_id: parent_id.to_string(),
        flags,
        tracestate: None,
    })
}

/// Extract trace context from HTTP header values, looking at both
/// `traceparent` and (optionally) `tracestate`. A malformed
/// `traceparent` makes the whole extraction fail (per the W3C
/// spec's "drop" rule).
pub fn extract_from_headers(
    traceparent: Option<&str>,
    tracestate: Option<&str>,
) -> TraceContextExtraction {
    let raw = match traceparent {
        Some(v) if !v.is_empty() => v,
        _ => return TraceContextExtraction::Absent,
    };
    match extract_traceparent(raw) {
        TraceContextExtraction::Present(mut ctx) => {
            ctx.tracestate = tracestate.map(str::to_string);
            TraceContextExtraction::Present(ctx)
        }
        other => other,
    }
}

/// Generate a fresh root span id (16 lowercase hex chars).
pub fn new_span_id() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_lower(&bytes)
}

/// Generate a fresh root trace id (32 lowercase hex chars).
pub fn new_trace_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_lower(&bytes)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Tracks the per-process trace-id generator. We use a `Mutex` so we
/// can guarantee the trace id is unique within the process even when
/// the OS clock is monotonic-broken (e.g. inside containers).
#[derive(Clone, Default)]
pub struct TraceIdGenerator {
    state: Arc<Mutex<TraceIdState>>,
}

#[derive(Default)]
struct TraceIdState {
    last_random_bytes: [u8; 16],
    initialized: bool,
}

impl TraceIdGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a freshly-minted trace id, distinct from the previous
    /// one. Cheap (~one RNG call) and good enough for Phase 4; the
    /// design mentions OTel SDK compatibility, which the random
    /// generator provides.
    pub fn next(&self) -> String {
        let mut state = self.state.lock();
        let mut bytes = [0u8; 16];
        if state.initialized {
            // Bump the last random value by 1; if the result equals
            // the previous, fall through to RNG. This guarantees
            // uniqueness on every call even in the unlikely event
            // that the OS RNG produces the same value twice.
            for i in (0..16).rev() {
                let (v, carry) = state.last_random_bytes[i].overflowing_add(1);
                state.last_random_bytes[i] = v;
                if !carry {
                    bytes = state.last_random_bytes;
                    break;
                }
            }
            if bytes == [0u8; 16] {
                rand::thread_rng().fill_bytes(&mut state.last_random_bytes);
                bytes = state.last_random_bytes;
            }
        } else {
            rand::thread_rng().fill_bytes(&mut state.last_random_bytes);
            bytes = state.last_random_bytes;
            state.initialized = true;
        }
        hex_lower(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_valid_traceparent() {
        let v = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        match extract_traceparent(v) {
            TraceContextExtraction::Present(c) => {
                assert_eq!(c.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
                assert_eq!(c.parent_span_id, "00f067aa0ba902b7");
                assert_eq!(c.flags, 0x01);
            }
            _ => panic!("expected present"),
        }
    }

    #[test]
    fn extract_rejects_wrong_version() {
        let v = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(extract_traceparent(v), TraceContextExtraction::Absent);
    }

    #[test]
    fn extract_rejects_wrong_length() {
        let v = "00-abc-00f067aa0ba902b7-01";
        assert_eq!(extract_traceparent(v), TraceContextExtraction::Absent);
    }

    #[test]
    fn extract_rejects_uppercase_hex() {
        // The spec is lowercase-only; uppercase is rejected.
        let v = "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01";
        assert_eq!(extract_traceparent(v), TraceContextExtraction::Absent);
    }

    #[test]
    fn extract_rejects_non_hex() {
        let v = "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01";
        assert_eq!(extract_traceparent(v), TraceContextExtraction::Absent);
    }

    #[test]
    fn extract_rejects_malformed_flags() {
        let v = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz";
        assert_eq!(extract_traceparent(v), TraceContextExtraction::Absent);
    }

    #[test]
    fn extract_absent_when_no_header() {
        assert_eq!(
            extract_from_headers(None, None),
            TraceContextExtraction::Absent
        );
    }

    #[test]
    fn extract_forwards_tracestate() {
        let v = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let extracted = extract_from_headers(Some(v), Some("vendor=abc"));
        match extracted {
            TraceContextExtraction::Present(c) => {
                assert_eq!(c.tracestate.as_deref(), Some("vendor=abc"));
            }
            _ => panic!("expected present"),
        }
    }

    #[test]
    fn new_span_id_has_correct_length() {
        let s = new_span_id();
        assert_eq!(s.len(), SPAN_ID_LEN);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn trace_id_generator_returns_distinct_values() {
        let g = TraceIdGenerator::new();
        let a = g.next();
        let b = g.next();
        assert_ne!(a, b);
        assert_eq!(a.len(), TRACE_ID_LEN);
    }
}
