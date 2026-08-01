// SPDX-License-Identifier: BSL-1.1

//! `IdentityBackend` — the full identity surface a multi-tenant registry needs.
//!
//! The `DatabaseRegistry` (Enterprise, multi-tenant) needs both grant-based
//! authorization ([`AuthorizationPolicy`]) and the database catalogue
//! ([`DatabaseCatalog`]) to bind a session: it resolves effective access on
//! `acquire` and looks the database up in the catalogue. Its admin/test entry
//! points also reach user management ([`UserStore`]). Rather than thread three
//! separate trait objects through the registry, this supertrait bundles them
//! into one `Arc<dyn IdentityBackend>`.
//!
//! This does **not** re-couple the Community authentication path: the local
//! auth provider ([`super::LocalAuthProvider`]) depends only on
//! [`UserStore`] and knows nothing of this supertrait. `IdentityBackend` is
//! the *registry's* dependency — and the registry itself is the Enterprise,
//! multi-tenant component, where converging the three capabilities is exactly
//! right. A single-database Community server uses a different registry that
//! needs no identity backend at all (see the split's `CommunityRegistry`).
//!
//! A blanket impl means every type that already implements the three
//! constituent traits — `SystemGraphAuthStore` does — is an `IdentityBackend`
//! automatically, with no extra code.

use super::{AuthorizationPolicy, DatabaseCatalog, UserStore};

/// The combined identity surface consumed by the multi-tenant registry:
/// user management + grant authorization + database catalogue.
pub trait IdentityBackend: UserStore + AuthorizationPolicy + DatabaseCatalog {}

/// Blanket impl: anything implementing all three constituent traits is an
/// `IdentityBackend`. No type needs to name this trait explicitly.
impl<T: UserStore + AuthorizationPolicy + DatabaseCatalog> IdentityBackend for T {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Object-safety: `IdentityBackend` must be usable as `dyn` so the
    /// registry can hold `Arc<dyn IdentityBackend>`. This function only needs
    /// to type-check.
    #[allow(dead_code)]
    fn assert_object_safe(_backend: &dyn IdentityBackend) {}

    /// Blanket-impl reachability: any type implementing the three constituent
    /// traits also satisfies `IdentityBackend` (via the blanket impl). This
    /// generic function only compiles if that implication holds.
    #[allow(dead_code)]
    fn assert_blanket<T: UserStore + AuthorizationPolicy + DatabaseCatalog>() {
        fn takes_backend<B: IdentityBackend>() {}
        takes_backend::<T>();
    }
}
