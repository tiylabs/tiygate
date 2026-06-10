//! TiyGate Cache — Embedding-only caching layer.
//!
//! Provides deterministic caching for embeddings (same input → same output).
//! Chat/completion caching is explicitly excluded.

pub mod embedding_cache;
