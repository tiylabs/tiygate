//! W3C Trace Context helpers — stable re-export surface from
//! `tiygate_core::tracing_ctx`.
//!
//! External consumers (tests, admin, future gateway extensions)
//! should import trace-context types through `crate::trace` rather
//! than reaching directly into `tiygate_core::tracing_ctx`. This
//! keeps the public API surface consistent even if the internal
//! module layout changes.
//!
//! The data-plane ingress handlers (`ingress.rs`, `ingress_phase4.rs`)
//! currently import directly from `tiygate_core::tracing_ctx` for
//! simplicity; this module is the canonical home for the re-exports
//! and is intentionally kept even though it is not yet used by
//! internal code paths.

// All re-exports are intentionally public; they are consumed by
// external callers that import from `tiygate_server::trace`. The
// internal gateway code imports from `tiygate_core` directly.
#[allow(unused_imports)]
pub use tiygate_core::tracing_ctx::TraceContext as PublicTraceContext;
#[allow(unused_imports)]
pub use tiygate_core::tracing_ctx::{
    extract_from_headers, extract_traceparent, new_span_id, new_trace_id, TraceContext,
    TraceContextExtraction, TraceIdGenerator, SPAN_ID_LEN, TRACE_ID_LEN,
};
