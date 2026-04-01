// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Server-wide concurrent cache for parsed query ASTs.
//!
//! Uses [`moka::sync::Cache`] which provides lock-free concurrent reads
//! via internal sharding. No external `RwLock` needed — multiple Bolt
//! connections can hit the cache simultaneously without contention.

use tessera_graph::GqlStatement;

/// Server-wide concurrent cache for parsed query ASTs.
///
/// Backed by `moka::sync::Cache` with internal sharding for high-concurrency
/// reads. Cache hits return a clone of the stored `GqlStatement`.
pub struct QueryCache {
    inner: moka::sync::Cache<String, GqlStatement>,
}

impl QueryCache {
    /// Create a new cache with the given maximum capacity.
    ///
    /// `capacity` is clamped to at least 1.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: moka::sync::Cache::builder()
                .max_capacity(capacity.max(1) as u64)
                .build(),
        }
    }

    /// Look up a cached AST by query text.
    ///
    /// Returns `None` on cache miss. Lock-free concurrent reads.
    #[must_use]
    pub fn get(&self, query: &str) -> Option<GqlStatement> {
        self.inner.get(query)
    }

    /// Insert a parsed statement into the cache.
    ///
    /// If the cache is at capacity, the least-recently-used entry is evicted.
    pub fn insert(&self, query: String, stmt: GqlStatement) {
        self.inner.insert(query, stmt);
    }

    /// Force pending eviction tasks to run synchronously.
    ///
    /// Only needed in tests — moka eviction is asynchronous by default.
    #[cfg(test)]
    fn sync_eviction(&self) {
        self.inner.run_pending_tasks();
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
    fn eviction_occurs_when_over_capacity() {
        // moka eviction is async/approximate — use a larger overshoot to
        // guarantee at least some entries are evicted after sync.
        let cache = QueryCache::new(4);
        let stmt = tessera_graph::gql::parse_statement("MATCH (n) RETURN n").unwrap(); // OK: test
        for i in 0..20 {
            cache.insert(format!("q{i}"), stmt.clone());
        }
        cache.sync_eviction();
        // After eviction, at most 4 entries should remain.
        let present = (0..20).filter(|i| cache.get(&format!("q{i}")).is_some()).count();
        assert!(present <= 8, "expected at most ~capacity entries, got {present}");
        assert!(present >= 1, "cache should still hold recent entries");
    }

    #[test]
    fn zero_capacity_clamps_to_one() {
        let cache = QueryCache::new(0);
        let stmt = tessera_graph::gql::parse_statement("MATCH (n) RETURN n").unwrap(); // OK: test
        cache.insert("q".to_owned(), stmt.clone());
        assert_eq!(cache.get("q").unwrap(), stmt); // OK: test
    }
}
