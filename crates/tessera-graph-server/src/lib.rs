// SPDX-License-Identifier: BSL-1.1

//! `TesseraGraph` Server — standalone Bolt 4.4 graph database.
//!
//! This crate provides a minimal, extensible Bolt server without enterprise
//! features (RBAC, LBAC, audit, multi-tenancy). Extension points via
//! traits (soon to return as async `AuthProvider` + `AuthStore`; see the
//! Fase 1a plan) and [`GraphAccessor`] allow the enterprise edition to
//! inject advanced security and multi-tenant behaviour.

/// El punto de extensión del despacho administrativo: sólo la forma, más la
/// implementación de la edición pública. Viaja al árbol público.
pub mod admin_dispatch;
/// Las seis sentencias de cuentas locales. Públicas por decisión de producto:
/// la autenticación básica no se esconde tras el muro de pago.
pub mod admin_users;
pub mod audit;
pub mod auth;
/// Lock-contention benchmark harness (Scenario 1, in-process). Gated behind the
/// `bench-support` feature so it never links into the released library or
/// binary. See `.private/tdd-plan-lock-contention-bench.md`.
#[cfg(feature = "bench-support")]
pub mod bench_support;
pub mod call_handler;
pub mod config;
/// La forma de los ajustes del gestor multi-base. Viaja al árbol público
/// porque la configuración común los transporta; la factoría que los lee, no.
pub mod config_paid_settings;
pub mod ddl_handler;
/// Lo que el binario necesita saber de su edición: por qué puerta arranca y
/// quién lee los ajustes del gestor multi-base. **Enterprise**: el árbol
/// público trae su propia versión, que ocupa este mismo hueco al copiar.
pub mod edition_community;
pub mod error;
pub mod graph_accessor;
pub mod handler;
/// Las vías de pago del manejador de sesión: copia de seguridad en caliente.
/// **Enterprise**: separadas de `handler`, que es el camino de consulta
/// público. Sale del árbol público en la mudanza.
pub(crate) mod handler_enterprise;
pub mod listener;
pub mod metrics;
pub mod migration;
pub mod params;
pub mod rate_limited_io;
pub mod rate_limiter;
pub mod registry;
/// El hueco por el que una sesión transporta el gestor de pago. Viaja al árbol
/// público; ver el fichero para qué cambia al copiar.
pub mod registry_handle;
pub mod startup;
pub mod system_lock;
pub mod wire;

pub use audit::{AuditBackend, AuditSink};
pub use auth::{
    AuthError, AuthOutcome, AuthProvider, AuthStoreError, NoAuthProvider, SecretString,
    SystemGraphAuthProvider, SystemGraphAuthStore, UserSummary,
};
pub use config::ServerConfig;
pub use error::{Result, ServerError};
pub use graph_accessor::{DefaultGraphAccessor, GraphAccessor};
/// Re-exported only for the test targets that inject a `GraphAccessor` double
/// through [`BoltHandler::with_accessor_factory`]; gated like that builder so
/// the released artefact does not widen its public surface.
#[cfg(any(test, feature = "test-util"))]
pub use handler::AccessorFactory;
pub use handler::{BoltHandler, sanitize_engine_error_for_wire};
pub use listener::TesseraListener;
/// Superficie de arranque neutra: `start_server_with_registry` recibe el gestor
/// por factoría en vez de construirlo, así que el mismo binario sirve a las dos
/// ediciones. Community le pasa `single_database_factory`; la de pago, la suya.
/// Es la única puerta de arranque que viaja al árbol público.
pub use startup::{
    RegistryBundle, RegistryFactory, single_database_factory, start_server_with_registry,
};
pub use startup::{ServerHandle, ServerReady};
