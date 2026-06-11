//! Pluggable log sink surface.
//!
//! Phase 1 shipped a `StdoutSink`; Phase 4 (产品化) adds the
//! [`oltp::OltpSink`] that writes events to the `request_logs` table
//! defined by `migrations/log/20260101000001_init.sql`. The two
//! sinks can be combined (fan-out) to keep a JSON-lines log on
//! disk while also feeding the OLTP store.

pub mod oltp;
pub mod stdout;
