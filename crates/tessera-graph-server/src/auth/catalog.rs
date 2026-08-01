// SPDX-License-Identifier: BSL-1.1

//! `DatabaseCatalog` — multi-database catalogue (**Enterprise edition**).
//!
//! This trait covers creating, dropping, and listing named databases: the
//! multi-tenancy surface. Like [`super::AuthorizationPolicy`], it is defined
//! in the Community server so the open code can hold a trait object, but the
//! Community edition ships no real implementation — a Community server serves
//! a single database and has no catalogue to manage. The real catalogue,
//! backed by the system graph, lives in the separate Enterprise repository,
//! so this is public API rather than `pub(crate)`.

use async_trait::async_trait;

use super::AuthStoreError;

// Igual que en `authorization.rs`: los tipos del catálogo acompañan a su
// interfaz en vez de quedarse en el fichero de autenticación.

/// Options used by `CREATE DATABASE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseOptions {
    pub max_size_bytes: Option<u64>,
    pub max_connections: Option<usize>,
}

/// Database catalog row as stored in the system graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub name: String,
    pub created_at: String,
    pub created_by: String,
    pub max_size_bytes: Option<u64>,
    pub max_connections: Option<usize>,
}

/// Named-database catalogue for multi-tenancy. **Enterprise edition.**
///
/// Object-safe + async, held as `Arc<dyn DatabaseCatalog>`.
#[async_trait]
pub trait DatabaseCatalog: Send + Sync + 'static {
    async fn create_database(
        &self,
        name: &str,
        options: DatabaseOptions,
        created_by: &str,
    ) -> Result<(), AuthStoreError>;

    async fn drop_database(&self, name: &str) -> Result<(), AuthStoreError>;

    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, AuthStoreError>;

    async fn get_database(&self, name: &str) -> Result<Option<DatabaseInfo>, AuthStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubCatalog;

    #[async_trait]
    impl DatabaseCatalog for StubCatalog {
        async fn create_database(
            &self,
            _name: &str,
            _options: DatabaseOptions,
            _created_by: &str,
        ) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn drop_database(&self, _name: &str) -> Result<(), AuthStoreError> {
            unimplemented!()
        }
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, AuthStoreError> {
            unimplemented!()
        }
        async fn get_database(
            &self,
            _name: &str,
        ) -> Result<Option<DatabaseInfo>, AuthStoreError> {
            unimplemented!()
        }
    }

    fn assert_object_safe(_catalog: &dyn DatabaseCatalog) {}

    /// Compile-time contract: `DatabaseCatalog` must stay object-safe so it
    /// can be held as `Arc<dyn DatabaseCatalog>`.
    #[test]
    fn database_catalog_is_object_safe() {
        assert_object_safe(&StubCatalog);
    }
}
