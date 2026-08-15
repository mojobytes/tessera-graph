// SPDX-License-Identifier: BSL-1.1

//! Authentication primitives.
//!
//! Fase 1a is being landed incrementally:
//! - Task 2: [`SecretString`] newtype.
//! - Task 3: argon2id hashing helpers.
//! - Task 4 (this): async [`AuthProvider`] / [`AuthStore`] traits + [`NoAuthProvider`].
//! - Task 5: `SystemGraphAuthProvider` / `SystemGraphAuthStore`.
//!
//! See `docs/superpowers/specs/2026-04-21-auth-argon2-multi-user-design.md`.

pub mod authorization;
pub mod catalog;
pub mod identity_backend;
pub mod local_provider;
pub mod noop;
pub mod password;
pub mod secret;
pub mod system_graph;
pub mod traits;
pub mod user_store;

pub use noop::NoAuthProvider;
pub use password::{
    MAX_PASSWORD_LEN, MIN_PASSWORD_LEN, PasswordError, hash_password, verify_password,
};
pub use secret::SecretString;
pub use system_graph::{SystemGraphAuthProvider, SystemGraphAuthStore, UserRecord};
pub use traits::{AccessLevel, AuthError, AuthOutcome, AuthProvider, AuthStoreError, UserSummary};
// Los tipos de datos de permisos y catálogo se reexportan desde la interfaz que
// los usa, no desde `traits`: allí convivían con los de autenticación, que sí
// son Community, y un fichero público acababa describiendo la forma de las
// sentencias de pago.
pub use authorization::{AuthorizationPolicy, Grant, GrantTarget, GrantTargetName};
pub use catalog::{DatabaseCatalog, DatabaseInfo, DatabaseOptions};
pub use identity_backend::IdentityBackend;
pub use local_provider::LocalAuthProvider;
pub use user_store::{UserAuthRecord, UserStore};
