// SPDX-License-Identifier: BSL-1.1

//! Server-wide concurrent cache for parsed query ASTs.
//!
//! Uses [`moka::sync::Cache`] which provides lock-free concurrent reads
//! via internal sharding. No external `RwLock` needed — multiple Bolt
//! connections can hit the cache simultaneously without contention.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use tessera_graph::GqlStatement;
use tessera_graph::gql::GqlValue;

/// Composite key identifying a cached parsed AST.
///
/// The cache used to be keyed solely on the raw query string, but with
/// `$param` substitution at the AST level a single text query produces
/// different ASTs depending on its bindings. The `params_signature` field
/// captures a deterministic hash of `(key, value)` pairs so two RUNs of
/// the same query with different bindings do not alias.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// The original query text exactly as received over the wire.
    pub query: String,
    /// Hash of the param map. `0` denotes the empty map.
    pub params_signature: u64,
}

/// Deterministic hash of a `$param` binding map.
///
/// Empty maps hash to `0`. Otherwise, entries are sorted by key (so the
/// `HashMap`'s insertion order is irrelevant) and fed in order into
/// `std::collections::hash_map::DefaultHasher`. Each `GqlValue` variant
/// is prefixed with a discriminant byte so two values with the same
/// bit-pattern but different runtime types do not collide (e.g.
/// `Int(0)` vs `Bool(false)`). Float values are hashed via `to_bits` so
/// `NaN` and `-0.0` are stable.
///
/// The result is intended to be combined with the query text in a
/// [`CacheKey`] for the in-process query cache. It is **not** a
/// cryptographic digest, **not** stable across Rust compiler versions
/// (the `DefaultHasher` algorithm is documented by the standard library
/// as implementation-defined), and **not** suitable for persistence,
/// inter-process exchange, or distributed caches. Switch to a stable
/// hasher (e.g. `fnv`, `xxhash`) if any of those use cases arise.
#[must_use]
pub fn hash_params<S: std::hash::BuildHasher>(params: &HashMap<String, GqlValue, S>) -> u64 {
    if params.is_empty() {
        return 0;
    }
    let mut entries: Vec<(&String, &GqlValue)> = params.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (k, v) in entries {
        k.hash(&mut hasher);
        hash_value(v, &mut hasher);
    }
    hasher.finish()
}

fn hash_value(v: &GqlValue, hasher: &mut impl Hasher) {
    match v {
        GqlValue::Null => 0u8.hash(hasher),
        GqlValue::Bool(b) => {
            1u8.hash(hasher);
            b.hash(hasher);
        }
        GqlValue::Int(i) => {
            2u8.hash(hasher);
            i.hash(hasher);
        }
        GqlValue::Float(f) => {
            3u8.hash(hasher);
            f.to_bits().hash(hasher);
        }
        GqlValue::Str(s) => {
            4u8.hash(hasher);
            s.hash(hasher);
        }
        GqlValue::List(items) => {
            5u8.hash(hasher);
            (items.len() as u64).hash(hasher);
            for item in items {
                hash_value(item, hasher);
            }
        }
        GqlValue::Map(m) => {
            6u8.hash(hasher);
            (m.len() as u64).hash(hasher);
            // Sort keys so the hash is independent of HashMap iteration order —
            // the same map must always produce the same cache key.
            let mut sorted_keys: Vec<&String> = m.keys().collect();
            sorted_keys.sort();
            for k in sorted_keys {
                k.hash(hasher);
                hash_value(&m[k], hasher);
            }
        }
        // Entity values are keyed by their stable id. Real Bolt serialisation
        // is added in a later task; this is the conservative cache-key form.
        GqlValue::Node(n) => {
            7u8.hash(hasher);
            n.id.hash(hasher);
        }
        GqlValue::Relationship(r) => {
            8u8.hash(hasher);
            r.id.hash(hasher);
        }
        GqlValue::Path(p) => {
            // Hash node AND relationship ids so two paths between the same
            // nodes via different edges (multi-edges) get distinct keys,
            // matching `gql_value_slice_to_key`.
            9u8.hash(hasher);
            for n in &p.nodes {
                n.id.hash(hasher);
            }
            for r in &p.rels {
                r.id.hash(hasher);
            }
        }
    }
}

/// Server-wide concurrent cache for parsed query ASTs.
///
/// Backed by `moka::sync::Cache` with internal sharding for high-concurrency
/// reads. Cache hits return a clone of the stored `GqlStatement`.
pub struct QueryCache {
    inner: moka::sync::Cache<CacheKey, GqlStatement>,
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

    /// Look up a cached AST by composite key.
    ///
    /// Returns `None` on cache miss. Lock-free concurrent reads.
    #[must_use]
    pub fn get(&self, key: &CacheKey) -> Option<GqlStatement> {
        self.inner.get(key)
    }

    /// Insert a parsed statement into the cache.
    ///
    /// If the cache is at capacity, the least-recently-used entry is evicted.
    pub fn insert(&self, key: CacheKey, stmt: GqlStatement) {
        self.inner.insert(key, stmt);
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

    fn key(q: &str) -> CacheKey {
        CacheKey {
            query: q.to_owned(),
            params_signature: 0,
        }
    }

    #[test]
    fn cache_miss_returns_none() {
        let cache = QueryCache::new(16);
        assert!(cache.get(&key("MATCH (n) RETURN n")).is_none());
    }

    #[test]
    fn cache_hit_returns_cloned_statement() {
        let cache = QueryCache::new(16);
        let q = "MATCH (n) RETURN n";
        let stmt = tessera_graph::gql::parse_statement(q).unwrap(); // OK: test
        cache.insert(key(q), stmt.clone());
        assert_eq!(cache.get(&key(q)).unwrap(), stmt); // OK: test
    }

    #[test]
    fn eviction_occurs_when_over_capacity() {
        // moka eviction is async/approximate — use a larger overshoot to
        // guarantee at least some entries are evicted after sync.
        let cache = QueryCache::new(4);
        let stmt = tessera_graph::gql::parse_statement("MATCH (n) RETURN n").unwrap(); // OK: test
        for i in 0..20 {
            cache.insert(key(&format!("q{i}")), stmt.clone());
        }
        cache.sync_eviction();
        // After eviction, at most 4 entries should remain.
        let present = (0..20)
            .filter(|i| cache.get(&key(&format!("q{i}"))).is_some())
            .count();
        assert!(
            present <= 8,
            "expected at most ~capacity entries, got {present}"
        );
        assert!(present >= 1, "cache should still hold recent entries");
    }

    #[test]
    fn zero_capacity_clamps_to_one() {
        let cache = QueryCache::new(0);
        let stmt = tessera_graph::gql::parse_statement("MATCH (n) RETURN n").unwrap(); // OK: test
        cache.insert(key("q"), stmt.clone());
        assert_eq!(cache.get(&key("q")).unwrap(), stmt); // OK: test
    }

    // ── Cycle 12: params_signature semantics ────────────────────────────

    #[test]
    fn cache_same_query_same_params_is_hit() {
        let cache = QueryCache::new(16);
        let q = "RETURN $x";
        let stmt = tessera_graph::gql::parse_statement("RETURN 1").unwrap(); // OK: test
        let mut params = HashMap::new();
        params.insert("x".to_owned(), GqlValue::Int(1));
        let sig = hash_params(&params);
        cache.insert(
            CacheKey {
                query: q.to_owned(),
                params_signature: sig,
            },
            stmt.clone(),
        );
        assert_eq!(
            cache
                .get(&CacheKey {
                    query: q.to_owned(),
                    params_signature: sig
                })
                .unwrap(), // OK: test
            stmt,
        );
    }

    #[test]
    fn cache_same_query_different_params_is_miss() {
        let cache = QueryCache::new(16);
        let q = "RETURN $x";
        let stmt = tessera_graph::gql::parse_statement("RETURN 1").unwrap(); // OK: test
        let mut a = HashMap::new();
        a.insert("x".to_owned(), GqlValue::Int(1));
        let mut b = HashMap::new();
        b.insert("x".to_owned(), GqlValue::Int(2));
        cache.insert(
            CacheKey {
                query: q.to_owned(),
                params_signature: hash_params(&a),
            },
            stmt,
        );
        assert!(
            cache
                .get(&CacheKey {
                    query: q.to_owned(),
                    params_signature: hash_params(&b)
                })
                .is_none(),
            "different param values must miss the cache",
        );
    }

    #[test]
    fn hash_params_empty_is_zero() {
        let empty: HashMap<String, GqlValue> = HashMap::new();
        assert_eq!(hash_params(&empty), 0);
    }

    #[test]
    fn hash_params_is_order_independent() {
        let mut a = HashMap::new();
        a.insert("x".to_owned(), GqlValue::Int(1));
        a.insert("y".to_owned(), GqlValue::Int(2));
        let mut b = HashMap::new();
        b.insert("y".to_owned(), GqlValue::Int(2));
        b.insert("x".to_owned(), GqlValue::Int(1));
        assert_eq!(hash_params(&a), hash_params(&b));
    }

    #[test]
    fn hash_params_distinguishes_type_with_same_bits() {
        // GqlValue::Int(0) vs GqlValue::Bool(false) — both "zero" bits but
        // different runtime types. The discriminant prefix in `hash_value`
        // must keep them apart so substitution semantics never alias.
        let mut a = HashMap::new();
        a.insert("k".to_owned(), GqlValue::Int(0));
        let mut b = HashMap::new();
        b.insert("k".to_owned(), GqlValue::Bool(false));
        assert_ne!(hash_params(&a), hash_params(&b));
    }
}
