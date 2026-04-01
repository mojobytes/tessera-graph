// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Server-wide LRU cache for parsed query ASTs.
//!
//! Thread-safe: multiple connections can read concurrently via `RwLock`.
//! `GqlStatement` is `Clone`, so cache hits return a cheap clone.

use std::num::NonZeroUsize;
use std::sync::RwLock;

use tessera_graph::GqlStatement;

/// Server-wide LRU cache for parsed query ASTs.
///
/// Wraps an `lru::LruCache` behind a `RwLock` so that concurrent Bolt
/// connections can share a single cache instance via `Arc<QueryCache>`.
///
/// # Note on `lru` crate advisory (RUSTSEC-2026-0002)
///
/// The `IterMut` unsoundness was reported against `lru 0.12.x`. This code
/// only uses `LruCache::get` and `LruCache::put` — `IterMut` is never
/// constructed. If a future change needs iteration, migrate to `moka` or
/// `quick_cache` first.
pub struct QueryCache {
    inner: RwLock<lru::LruCache<String, GqlStatement>>,
}

impl QueryCache {
    /// Create a new cache with the given maximum capacity.
    ///
    /// `capacity` is clamped to at least 1.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: RwLock::new(lru::LruCache::new(cap)),
        }
    }

    /// Look up a cached AST by query text.
    ///
    /// Returns `None` on cache miss. On hit, updates LRU recency and returns
    /// a clone of the stored `GqlStatement`.
    #[must_use]
    pub fn get(&self, query: &str) -> Option<GqlStatement> {
        // `lru::LruCache::get` updates recency and requires `&mut self`,
        // so we take a write lock even for reads.
        self.inner.write().ok()?.get(query).cloned()
    }

    /// Insert a parsed statement into the cache.
    ///
    /// If the cache is at capacity, the least-recently-used entry is evicted.
    pub fn insert(&self, query: String, stmt: GqlStatement) {
        if let Ok(mut guard) = self.inner.write() {
            guard.put(query, stmt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_miss_returns_none() {
        let cache = QueryCache::new(16);
        assert!(cache.get("MATCH (n) RETURN n").is_none());
    }

    #[test]
    fn cache_hit_returns_cloned_statement() {
        let cache = QueryCache::new(16);
        let q = "MATCH (n) RETURN n";
        let stmt = tessera_graph::gql::parse_statement(q).unwrap(); // OK: test
        cache.insert(q.to_owned(), stmt.clone());
        assert_eq!(cache.get(q).unwrap(), stmt); // OK: test
    }

    #[test]
    fn capacity_is_respected() {
        let cache = QueryCache::new(2);
        let s1 = tessera_graph::gql::parse_statement("MATCH (a) RETURN a").unwrap(); // OK: test
        let s2 = tessera_graph::gql::parse_statement("MATCH (b) RETURN b").unwrap(); // OK: test
        let s3 = tessera_graph::gql::parse_statement("MATCH (c) RETURN c").unwrap(); // OK: test
        cache.insert("q1".to_owned(), s1);
        cache.insert("q2".to_owned(), s2);
        cache.insert("q3".to_owned(), s3); // evicts q1
        assert!(cache.get("q1").is_none(), "q1 should be evicted");
        assert!(cache.get("q3").is_some(), "q3 should be present");
    }
}
