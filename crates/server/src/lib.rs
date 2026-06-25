//! TiyGate server library — exposes public modules for integration tests
//! and downstream binaries.

pub mod app;
pub mod config;
pub mod drain;
pub mod ingress;
pub mod models;
pub mod oauth_manager;
pub mod telemetry;
#[cfg(feature = "webui")]
pub mod webui;
// trace.rs is intentionally not yet re-exported; it is a stable
// re-export surface for tiygate_core::tracing_ctx, kept for
// external consumers who want canonical import paths. Internal
// code imports trace types directly from tiygate_core.
// pub mod trace;
