//! TiyGate Providers — Concrete provider implementations.
//!
//! Each provider implements the `Provider` trait from `tiygate_core`
//! and registers via `inventory::submit!`.

pub mod anthropic;
pub mod deepseek;
pub mod gemini;
pub mod moonshot;
pub mod oauth;
pub mod ollama;
pub mod openai;
pub mod openai_compatible;
pub mod xai;
pub mod zhipu;
