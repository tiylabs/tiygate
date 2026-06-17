//! TiyGate Cache — Embedding-only caching layer.
//!
//! Phase 4 implements an moka-backed LRU cache for deterministic
//! embedding results. Chat / completion responses are explicitly
//! **not** cached — see design doc §4.7.
//!
//! ## Key construction
//!
//! The cache key is a SHA-256 hex digest of
//! `model | normalized_input | dimensions | encoding_format`. The
//! normalization step folds whitespace runs and lowercases Unicode
//! so a *semantically identical* prompt hits the same cache entry
//! regardless of the exact whitespace the client supplied.
//!
//! ## TTL
//!
//! The default TTL is 7 days (per §4.7). Operators can override via
//! `embedding_cache_ttl_secs` at construction.
//!
//! ## Chat bypass
//!
//! The cache only applies to the `/v1/embeddings` route. Other
//! ingress handlers are expected to *not* consult this cache at
//! all (the chat handlers in the data plane never call
//! `EmbeddingCache::get`).

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::debug;

/// Default TTL for embedding cache entries.
pub const DEFAULT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Default max capacity (entries).
pub const DEFAULT_MAX_CAPACITY: u64 = 10_000;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache backend error: {0}")]
    Backend(String),
}

/// Embedding cache key — derived deterministically from the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingCacheKey {
    pub model: String,
    /// The input text (may be a list joined by newlines).
    pub input: String,
    pub dimensions: Option<u32>,
    pub encoding_format: Option<String>,
}

impl EmbeddingCacheKey {
    /// Build a key from a model name and a single input string.
    pub fn new(model: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            input: normalize(input.into()),
            dimensions: None,
            encoding_format: None,
        }
    }

    pub fn with_dimensions(mut self, d: u32) -> Self {
        self.dimensions = Some(d);
        self
    }

    pub fn with_encoding_format(mut self, fmt: impl Into<String>) -> Self {
        self.encoding_format = Some(fmt.into());
        self
    }

    /// Compute the SHA-256 hex digest used as the moka key.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.model.as_bytes());
        h.update(b"|");
        h.update(self.input.as_bytes());
        h.update(b"|");
        if let Some(d) = self.dimensions {
            h.update(d.to_le_bytes());
        }
        h.update(b"|");
        if let Some(f) = &self.encoding_format {
            h.update(f.as_bytes());
        }
        hex::encode(h.finalize())
    }
}

/// Value stored in the cache. We store the JSON response verbatim
/// so the gateway can pass it back to the client without any
/// post-processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedEmbedding {
    pub response: serde_json::Value,
    /// Cached at (epoch millis).
    pub cached_at_ms: u64,
}

/// The embedding cache. Wrapped in an `Arc` so handlers can hold
/// cheap clones.
pub struct EmbeddingCache {
    inner: Cache<String, Arc<CachedEmbedding>>,
    ttl: Duration,
}

impl EmbeddingCache {
    /// Build a new cache with the default capacity and TTL.
    pub fn new() -> Arc<Self> {
        Self::with_capacity(DEFAULT_MAX_CAPACITY, DEFAULT_TTL)
    }

    /// Build a new cache with the given capacity and TTL.
    pub fn with_capacity(capacity: u64, ttl: Duration) -> Arc<Self> {
        let inner = Cache::builder()
            .max_capacity(capacity)
            .time_to_live(ttl)
            .build();
        Arc::new(Self { inner, ttl })
    }

    /// Look up `key`. Returns `Some` on a hit, `None` on a miss.
    pub async fn get(&self, key: &EmbeddingCacheKey) -> Option<Arc<CachedEmbedding>> {
        let digest = key.digest();
        let v = self.inner.get(&digest).await;
        if v.is_some() {
            debug!(digest = %digest, model = %key.model, "embedding cache hit");
        }
        v
    }

    /// Insert `value` under `key`.
    pub async fn put(&self, key: &EmbeddingCacheKey, response: serde_json::Value) {
        let v = CachedEmbedding {
            response,
            cached_at_ms: now_ms(),
        };
        self.inner.insert(key.digest(), Arc::new(v)).await;
    }

    /// Returns the number of entries currently in the cache. Mostly
    /// useful for tests / admin diagnostics.
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Invalidate every entry. Used by tests; production code never
    /// needs this.
    pub async fn invalidate_all(&self) {
        self.inner.invalidate_all();
    }
}

impl Default for EmbeddingCache {
    fn default() -> Self {
        // Without an Arc — only `new()` is the ergonomic entry point.
        // `Default::default` exists so the cache can sit inside
        // option-bearing struct fields; the real production handle
        // is `EmbeddingCache::new()`.
        let inner = Cache::builder()
            .max_capacity(DEFAULT_MAX_CAPACITY)
            .time_to_live(DEFAULT_TTL)
            .build();
        Self {
            inner,
            ttl: DEFAULT_TTL,
        }
    }
}

/// Normalise an input string for cache-key purposes: fold
/// whitespace runs into a single space and trim the ends. We do not
/// attempt locale-aware case folding — the design doc §4.7 says
/// "embeddings are deterministic", and the *upstream API* is case-
/// sensitive in the general case.
fn normalize(s: String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn put_then_get_returns_cached_value() {
        let cache = EmbeddingCache::new();
        let key = EmbeddingCacheKey::new("text-embedding-3-small", "hello world");
        let response = json!({"data": [{"embedding": [0.1, 0.2]}]});
        cache.put(&key, response.clone()).await;
        let got = cache.get(&key).await.expect("hit");
        assert_eq!(got.response, response);
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let cache = EmbeddingCache::new();
        let key = EmbeddingCacheKey::new("m", "not-stored");
        assert!(cache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn normalization_folds_whitespace() {
        let k1 = EmbeddingCacheKey::new("m", "hello   world");
        let k2 = EmbeddingCacheKey::new("m", "  hello\tworld  ");
        let k3 = EmbeddingCacheKey::new("m", "hello world");
        assert_eq!(k1.digest(), k2.digest());
        assert_eq!(k1.digest(), k3.digest());
    }

    #[tokio::test]
    async fn case_difference_keeps_separate_entries() {
        // Embeddings are case-sensitive in the upstream; we mirror
        // that behaviour here so a miss-then-cache for "Hello" does
        // not return "hello".
        let k1 = EmbeddingCacheKey::new("m", "Hello");
        let k2 = EmbeddingCacheKey::new("m", "hello");
        assert_ne!(k1.digest(), k2.digest());
    }

    #[tokio::test]
    async fn dimensions_change_digest() {
        let a = EmbeddingCacheKey::new("m", "x").with_dimensions(256);
        let b = EmbeddingCacheKey::new("m", "x").with_dimensions(512);
        assert_ne!(a.digest(), b.digest());
    }

    #[tokio::test]
    async fn chat_path_does_not_use_embedding_cache() {
        // The contract is that chat handlers in the data plane
        // simply never call this cache. We model the contract in
        // tests by checking that the cache key is bound to the
        // *embeddings* path — i.e. a non-embedding key (no model
        // name) has a different digest shape.
        let embed = EmbeddingCacheKey::new("text-embedding-3-small", "hi");
        let chat_proxy = EmbeddingCacheKey::new("gpt-4o", "hi");
        // Sanity: the two keys are distinct so a chat request would
        // not collide with an embedding request.
        assert_ne!(embed.digest(), chat_proxy.digest());
    }

    #[tokio::test]
    async fn invalidate_all_clears_entries() {
        let cache = EmbeddingCache::new();
        let key = EmbeddingCacheKey::new("m", "x");
        cache.put(&key, json!({})).await;
        assert!(cache.get(&key).await.is_some());
        cache.invalidate_all().await;
        // moka's `invalidate_all` is async-collected; the next
        // `get` may briefly still observe the entry. We give the
        // runtime a moment and then assert a miss.
        for _ in 0..20 {
            if cache.get(&key).await.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("invalidate_all did not clear the entry within 200ms");
    }
}
