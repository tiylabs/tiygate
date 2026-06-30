//! TiyGate Protocols — Concrete protocol codec implementations.
//!
//! This crate provides codec implementations for various AI API protocols.
//! Each protocol implements the `EndpointCodec` trait from `tiygate_core`
//! and registers via `inventory::submit!`.

pub mod chat_completions;
pub mod embeddings;
pub mod error_body;
pub mod gemini;
pub mod images;
pub mod messages;
pub mod responses;
