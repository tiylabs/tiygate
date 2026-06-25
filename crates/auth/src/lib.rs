//! TiyGate Auth — `AuthApplier` implementations.
//!
//! Concrete authentication appliers (OAuth 2.0, API key, mTLS, …)
//! that implement the `AuthApplier` trait from `tiygate_core`.

pub mod api_key;
pub mod bearer;
pub mod oauth;
pub mod provider_oauth;
