// SPDX-License-Identifier: BSL-1.1

//! System-graph-backed **local user management** — the Community identity
//! surface. Every user is a `:User` node in a dedicated `Graph`, separate from
//! the user's data graph.
//!
//! Creating, dropping, re-passwording, enabling and listing users, plus the
//! credential lookup the local auth provider reads on every login. That is the
//! whole file.
//!
//! # What is not here
//!
//! Grants and the multi-database catalogue — both Enterprise — live in
//! `authorization_store.rs`. They operate on the same store state, reached
//! through the module-visible accessors below, so the two locks stay private
//! to the `auth` module.
//!
//! The two halves used to be one 1.255-line file implementing all three
//! surfaces. Moving that whole into the public Community repository would have
//! carried the paid ones with it, hidden in something whose name reads as
//! authentication.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use async_trait::async_trait;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use ermya_graph::{Graph, NodeId, Properties, Property};

// Local user management only. Grants, the catalogue and their types moved to
// `authorization_store.rs`; this import list is the visible line of the split.
use super::{
    AuthStoreError, MAX_PASSWORD_LEN, MIN_PASSWORD_LEN, SecretString, UserAuthRecord, UserStore,
    UserSummary, hash_password,
};

// Etiquetas de la parte de identidad del grafo de sistema.
//
// Las de catálogo y permisos no están: sólo las nombra el fichero que
// implementa esas operaciones, que no viaja a esta edición.
pub(super) const USER_LABEL: &str = "User";
pub(super) const WILDCARD_LABEL: &str = "Wildcard";

/// Precomputed argon2 hash used as a decoy on `authenticate` misses so
/// that unknown-user and wrong-password paths take the same wall-clock
/// time. Built lazily on first access.
pub(super) static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    let dummy = SecretString::new("dummy-for-timing".to_owned());
    hash_password(&dummy).expect("compile-time dummy must hash")
});

/// In-memory mirror of a stored user. Read-hot path lookups hit this
/// map instead of touching the graph engine.
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub enabled: bool,
    pub is_admin: bool,
    pub created_at: String,
    pub node_id: NodeId,
}

/// Lowercase + trim a principal before lookup / comparison.
pub(super) fn normalise_username(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

pub(super) fn validate_username(raw: &str) -> Result<String, AuthStoreError> {
    let norm = normalise_username(raw);
    if norm.is_empty() {
        return Err(AuthStoreError::InvalidUsername {
            reason: "empty".to_owned(),
        });
    }
    if norm.len() > 63 {
        return Err(AuthStoreError::InvalidUsername {
            reason: format!("too long ({}>63 chars)", norm.len()),
        });
    }
    let mut chars = norm.chars();
    let first = chars.next().expect("non-empty");
    if !(first.is_ascii_lowercase() || first.is_ascii_digit() || first == '_') {
        return Err(AuthStoreError::InvalidUsername {
            reason: format!("first char {first:?} not in [a-z0-9_]"),
        });
    }
    for c in chars {
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-');
        if !ok {
            return Err(AuthStoreError::InvalidUsername {
                reason: format!("char {c:?} not in [a-z0-9_.-]"),
            });
        }
    }
    Ok(norm)
}

fn validate_password(pw: &SecretString) -> Result<(), AuthStoreError> {
    if pw.is_empty() {
        return Err(AuthStoreError::PasswordEmpty);
    }
    if pw.len() < MIN_PASSWORD_LEN {
        return Err(AuthStoreError::PasswordTooShort {
            min: MIN_PASSWORD_LEN,
        });
    }
    if pw.len() > MAX_PASSWORD_LEN {
        return Err(AuthStoreError::PasswordTooLong {
            max: MAX_PASSWORD_LEN,
        });
    }
    Ok(())
}

pub(super) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub(super) fn string_prop(props: &Properties, key: &str) -> Option<String> {
    match props.get(key) {
        Some(Property::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn bool_prop(props: &Properties, key: &str) -> Option<bool> {
    match props.get(key) {
        Some(Property::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Identity store backed by a dedicated `ermya_graph::Graph`.
///
/// Wraps the graph in an `Arc<RwLock<Graph>>` for shared ownership and
/// holds an in-memory `HashMap<normalised_username, UserRecord>` mirror
/// so the hot auth path never touches the engine.
///
/// Write-through semantics: every mutation hits the graph first and the
/// map second on success.
///
/// # Lock ordering invariant
///
/// Methods that need both locks MUST acquire them in the order
/// `users` → `graph` and release `users` before taking `graph.write()`
/// (the in-memory map is a short-lived lookup; the graph write can
/// take milliseconds under WAL fsync). Any new method that inverts
/// this order risks a deadlock under contention.
pub struct SystemGraphAuthStore {
    graph: Arc<RwLock<Graph>>,
    users: RwLock<HashMap<String, UserRecord>>,
}

impl SystemGraphAuthStore {
    /// Open a new store against `graph`, loading existing `:User` nodes
    /// into the in-memory map.
    ///
    /// # Errors
    ///
    /// Returns [`AuthStoreError::Backend`] if the graph cannot be read
    /// (poisoned lock) or a stored user node is missing required
    /// properties.
    /// Opening the store does **not** seed anything for grants.
    ///
    /// It used to create the singleton node that wildcard grants attach to,
    /// unconditionally. That node only means something where grants exist, so
    /// a Community server was writing a node into its system graph that
    /// nothing in its edition would ever read. The Enterprise side seeds it
    /// explicitly when its identity bootstrap runs.
    pub fn new(graph: Arc<RwLock<Graph>>) -> Result<Self, AuthStoreError> {
        let users = Self::load_users(&graph)?;
        Ok(Self {
            graph,
            users: RwLock::new(users),
        })
    }

    fn load_users(
        graph: &Arc<RwLock<Graph>>,
    ) -> Result<HashMap<String, UserRecord>, AuthStoreError> {
        let g = graph
            .read()
            .map_err(|_| AuthStoreError::Backend("system graph lock poisoned".to_owned()))?;
        let mut out = HashMap::new();
        for id in g.nodes_by_label(USER_LABEL) {
            let node = g
                .node(id)
                .map_err(|e| AuthStoreError::Backend(e.to_string()))?;
            let props = node.properties();
            let username = string_prop(props, "username").ok_or_else(|| {
                AuthStoreError::Backend(format!("user node {id:?} missing username"))
            })?;
            let rec = UserRecord {
                id: string_prop(props, "id").unwrap_or_default(),
                username: username.clone(),
                password_hash: string_prop(props, "password_hash").unwrap_or_default(),
                enabled: bool_prop(props, "enabled").unwrap_or(true),
                is_admin: bool_prop(props, "is_admin").unwrap_or(false),
                created_at: string_prop(props, "created_at").unwrap_or_default(),
                node_id: node.id(),
            };
            out.insert(username, rec);
        }
        Ok(out)
    }

    /// Apply `(key, value)` property updates to the given `:User` node by
    /// reading it, mutating its properties, and writing it back with
    /// `Graph::update_node`. Centralises the "load-modify-store" pattern
    /// so the mutation methods stay narrow.
    fn apply_property_updates(
        &self,
        node_id: NodeId,
        updates: &[(&str, Property)],
    ) -> Result<(), AuthStoreError> {
        let mut g = self
            .graph
            .write()
            .map_err(|_| AuthStoreError::Backend("system graph lock poisoned".to_owned()))?;
        let mut node = g
            .node(node_id)
            .map_err(|e| AuthStoreError::Backend(e.to_string()))?;
        {
            let props = node.properties_mut();
            for (k, v) in updates {
                props.insert((*k).to_owned(), v.clone());
            }
        }
        g.update_node(node_id, &node)
            .map_err(|e| AuthStoreError::Backend(e.to_string()))?;
        Ok(())
    }

    /// Count the number of `:Wildcard` nodes currently in the graph.
    /// Used by bootstrap tests to assert idempotency.
    ///
    /// `async` is kept for call-site symmetry with the rest of the
    /// identity API (tests always `.await.unwrap()`); the body does
    /// no I/O beyond the in-memory label index.
    #[doc(hidden)]
    #[allow(clippy::unused_async)]
    pub async fn count_wildcard_nodes(&self) -> Result<usize, AuthStoreError> {
        let g = self
            .graph
            .read()
            .map_err(|_| AuthStoreError::Backend("system graph lock poisoned".to_owned()))?;
        Ok(g.nodes_by_label(WILDCARD_LABEL).len())
    }
}

/// Compatibility alias for the renamed [`super::LocalAuthProvider`].
///
/// The provider used to live here as `SystemGraphAuthProvider`, coupled to a
/// concrete `Arc<SystemGraphAuthStore>`. It has moved to `local_provider.rs`
/// as `LocalAuthProvider`, depending only on `Arc<dyn UserStore>` (the
/// Community authentication surface). This alias keeps existing call sites —
/// almost all in tests — compiling: `from_store(Arc<SystemGraphAuthStore>)`
/// still works because `SystemGraphAuthStore: UserStore` unsize-coerces to
/// `Arc<dyn UserStore>`. The alias is removed in the split's final wiring
/// cycle once call sites are updated to the new name.
pub type SystemGraphAuthProvider = super::LocalAuthProvider;

#[async_trait]
impl UserStore for SystemGraphAuthStore {
    async fn create_user(
        &self,
        username: &str,
        password_plain: &SecretString,
        is_admin: bool,
    ) -> Result<(), AuthStoreError> {
        let username = validate_username(username)?;
        validate_password(password_plain)?;

        {
            let users = self
                .users
                .read()
                .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
            if users.contains_key(&username) {
                return Err(AuthStoreError::UserExists(username));
            }
        }

        let phc =
            hash_password(password_plain).map_err(|e| AuthStoreError::Backend(e.to_string()))?;
        let id = Uuid::now_v7().to_string();
        let created_at = now_rfc3339();

        let node_id = {
            let mut g = self
                .graph
                .write()
                .map_err(|_| AuthStoreError::Backend("system graph lock poisoned".to_owned()))?;
            let mut props: Properties = HashMap::new();
            props.insert("id".to_owned(), Property::String(id.clone()));
            props.insert("username".to_owned(), Property::String(username.clone()));
            props.insert("password_hash".to_owned(), Property::String(phc.clone()));
            props.insert("enabled".to_owned(), Property::Bool(true));
            props.insert("is_admin".to_owned(), Property::Bool(is_admin));
            props.insert(
                "created_at".to_owned(),
                Property::String(created_at.clone()),
            );
            props.insert(
                "updated_at".to_owned(),
                Property::String(created_at.clone()),
            );
            g.add_node(USER_LABEL, props)
                .map_err(|e| AuthStoreError::Backend(e.to_string()))?
        };

        let rec = UserRecord {
            id,
            username: username.clone(),
            password_hash: phc,
            enabled: true,
            is_admin,
            created_at,
            node_id,
        };
        self.users
            .write()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?
            .insert(username, rec);
        Ok(())
    }

    async fn drop_user(&self, username: &str) -> Result<(), AuthStoreError> {
        let username = validate_username(username)?;
        let node_id = {
            let users = self
                .users
                .read()
                .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
            let rec = users
                .get(&username)
                .ok_or_else(|| AuthStoreError::UserNotFound(username.clone()))?;
            if rec.is_admin && rec.enabled {
                let other_admins_enabled = users
                    .values()
                    .any(|u| u.username != username && u.is_admin && u.enabled);
                if !other_admins_enabled {
                    return Err(AuthStoreError::LastAdmin);
                }
            }
            rec.node_id
        };

        {
            let mut g = self
                .graph
                .write()
                .map_err(|_| AuthStoreError::Backend("system graph lock poisoned".to_owned()))?;
            g.remove_node(node_id)
                .map_err(|e| AuthStoreError::Backend(e.to_string()))?;
        }
        self.users
            .write()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?
            .remove(&username);
        Ok(())
    }

    async fn set_password(
        &self,
        username: &str,
        password_plain: &SecretString,
    ) -> Result<(), AuthStoreError> {
        let username = validate_username(username)?;
        validate_password(password_plain)?;
        let phc =
            hash_password(password_plain).map_err(|e| AuthStoreError::Backend(e.to_string()))?;
        let updated_at = now_rfc3339();

        let node_id = {
            let users = self
                .users
                .read()
                .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
            users
                .get(&username)
                .ok_or_else(|| AuthStoreError::UserNotFound(username.clone()))?
                .node_id
        };

        self.apply_property_updates(
            node_id,
            &[
                ("password_hash", Property::String(phc.clone())),
                ("updated_at", Property::String(updated_at)),
            ],
        )?;

        if let Some(rec) = self
            .users
            .write()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?
            .get_mut(&username)
        {
            rec.password_hash = phc;
        }
        Ok(())
    }

    async fn set_enabled(&self, username: &str, enabled: bool) -> Result<(), AuthStoreError> {
        let username = validate_username(username)?;
        let node_id = {
            let users = self
                .users
                .read()
                .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
            let rec = users
                .get(&username)
                .ok_or_else(|| AuthStoreError::UserNotFound(username.clone()))?;
            if !enabled && rec.is_admin && rec.enabled {
                let other_admins_enabled = users
                    .values()
                    .any(|u| u.username != username && u.is_admin && u.enabled);
                if !other_admins_enabled {
                    return Err(AuthStoreError::LastAdmin);
                }
            }
            rec.node_id
        };
        let updated_at = now_rfc3339();

        self.apply_property_updates(
            node_id,
            &[
                ("enabled", Property::Bool(enabled)),
                ("updated_at", Property::String(updated_at)),
            ],
        )?;

        if let Some(rec) = self
            .users
            .write()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?
            .get_mut(&username)
        {
            rec.enabled = enabled;
        }
        Ok(())
    }

    async fn set_admin(&self, username: &str, is_admin: bool) -> Result<(), AuthStoreError> {
        let username = validate_username(username)?;
        let node_id = {
            let users = self
                .users
                .read()
                .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
            let rec = users
                .get(&username)
                .ok_or_else(|| AuthStoreError::UserNotFound(username.clone()))?;
            if !is_admin && rec.is_admin && rec.enabled {
                let other_admins_enabled = users
                    .values()
                    .any(|u| u.username != username && u.is_admin && u.enabled);
                if !other_admins_enabled {
                    return Err(AuthStoreError::LastAdmin);
                }
            }
            rec.node_id
        };
        let updated_at = now_rfc3339();

        self.apply_property_updates(
            node_id,
            &[
                ("is_admin", Property::Bool(is_admin)),
                ("updated_at", Property::String(updated_at)),
            ],
        )?;

        if let Some(rec) = self
            .users
            .write()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?
            .get_mut(&username)
        {
            rec.is_admin = is_admin;
        }
        Ok(())
    }

    async fn list_users(&self) -> Result<Vec<UserSummary>, AuthStoreError> {
        let users = self
            .users
            .read()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
        let mut out: Vec<UserSummary> = users
            .values()
            .map(|u| UserSummary {
                username: u.username.clone(),
                enabled: u.enabled,
                is_admin: u.is_admin,
                created_at: u.created_at.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    async fn get_user_for_auth(
        &self,
        username: &str,
    ) -> Result<Option<UserAuthRecord>, AuthStoreError> {
        let norm = normalise_username(username);
        let users = self
            .users
            .read()
            .map_err(|_| AuthStoreError::Backend("users lock poisoned".to_owned()))?;
        Ok(users.get(&norm).map(|u| UserAuthRecord {
            password_hash: u.password_hash.clone(),
            enabled: u.enabled,
            is_admin: u.is_admin,
            id: u.id.clone(),
        }))
    }
}

#[cfg(test)]
mod split_trait_contract_tests {
    use super::*;

    // Static assertion (compile-time only): this store implements the
    // **Community** identity surface. Fails to compile — not at runtime — if
    // the impl is dropped or a signature drifts. No `Graph` is constructed.
    //
    // Deliberately checks only `UserStore`. The two paid surfaces are
    // implemented in `authorization_store.rs`, which does not travel to the
    // public repository, and asserting them from here would leave a test that
    // cannot compile once the tree is split. Their counterpart assertion lives
    // beside their implementation.

    fn assert_user_store<T: UserStore>() {}

    #[test]
    fn system_graph_auth_store_implements_the_community_identity_surface() {
        assert_user_store::<SystemGraphAuthStore>();
    }
}
