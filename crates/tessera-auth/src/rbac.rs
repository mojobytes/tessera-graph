// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::error::{AuthError, Result};

/// Unique identifier for a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RoleId(u64);

impl RoleId {
    /// Create a `RoleId` from a raw value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the underlying numeric identifier.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Granular permission for a specific operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Permission {
    NodeCreate,
    NodeRead,
    NodeUpdate,
    NodeDelete,
    EdgeCreate,
    EdgeRead,
    EdgeUpdate,
    EdgeDelete,
    GraphFlush,
    GraphBackup,
    GraphConfig,
    AdminUsers,
    AdminRoles,
    AdminAudit,
    Monitor,
}

impl Permission {
    /// Return all permission variants.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::NodeCreate,
            Self::NodeRead,
            Self::NodeUpdate,
            Self::NodeDelete,
            Self::EdgeCreate,
            Self::EdgeRead,
            Self::EdgeUpdate,
            Self::EdgeDelete,
            Self::GraphFlush,
            Self::GraphBackup,
            Self::GraphConfig,
            Self::AdminUsers,
            Self::AdminRoles,
            Self::AdminAudit,
            Self::Monitor,
        ]
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::NodeCreate => "node:create",
            Self::NodeRead => "node:read",
            Self::NodeUpdate => "node:update",
            Self::NodeDelete => "node:delete",
            Self::EdgeCreate => "edge:create",
            Self::EdgeRead => "edge:read",
            Self::EdgeUpdate => "edge:update",
            Self::EdgeDelete => "edge:delete",
            Self::GraphFlush => "graph:flush",
            Self::GraphBackup => "graph:backup",
            Self::GraphConfig => "graph:config",
            Self::AdminUsers => "admin:users",
            Self::AdminRoles => "admin:roles",
            Self::AdminAudit => "admin:audit",
            Self::Monitor => "monitor",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for Permission {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "node:create" => Ok(Self::NodeCreate),
            "node:read" => Ok(Self::NodeRead),
            "node:update" => Ok(Self::NodeUpdate),
            "node:delete" => Ok(Self::NodeDelete),
            "edge:create" => Ok(Self::EdgeCreate),
            "edge:read" => Ok(Self::EdgeRead),
            "edge:update" => Ok(Self::EdgeUpdate),
            "edge:delete" => Ok(Self::EdgeDelete),
            "graph:flush" => Ok(Self::GraphFlush),
            "graph:backup" => Ok(Self::GraphBackup),
            "graph:config" => Ok(Self::GraphConfig),
            "admin:users" => Ok(Self::AdminUsers),
            "admin:roles" => Ok(Self::AdminRoles),
            "admin:audit" => Ok(Self::AdminAudit),
            "monitor" => Ok(Self::Monitor),
            other => Err(format!("unknown permission: {other}")),
        }
    }
}

/// A named set of permissions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Role {
    id: RoleId,
    name: String,
    permissions: HashSet<Permission>,
    #[serde(default)]
    predefined: bool,
}

impl Role {
    /// The role's display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The set of permissions this role grants.
    #[must_use]
    pub const fn permissions(&self) -> &HashSet<Permission> {
        &self.permissions
    }

    /// The role's unique identifier.
    #[must_use]
    pub const fn id(&self) -> RoleId {
        self.id
    }
}

/// Store of all roles (predefined + custom).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoleStore {
    roles: HashMap<RoleId, Role>,
    next_id: u64,
}

impl RoleStore {
    /// Predefined role IDs.
    pub const ADMIN_ROLE_ID: RoleId = RoleId(0);
    pub const READWRITE_ROLE_ID: RoleId = RoleId(1);
    pub const READONLY_ROLE_ID: RoleId = RoleId(2);
    pub const MONITOR_ROLE_ID: RoleId = RoleId(3);

    /// Number of predefined roles (IDs 0..3).
    const PREDEFINED_COUNT: u64 = 4;

    /// Create a role store populated with the four predefined roles.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut roles = HashMap::new();

        // Admin — all permissions
        roles.insert(
            Self::ADMIN_ROLE_ID,
            Role {
                id: Self::ADMIN_ROLE_ID,
                name: "admin".to_owned(),
                permissions: Permission::all().iter().copied().collect(),
                predefined: true,
            },
        );

        // Read-write — CRUD on nodes and edges
        let rw_perms: HashSet<Permission> = [
            Permission::NodeCreate,
            Permission::NodeRead,
            Permission::NodeUpdate,
            Permission::NodeDelete,
            Permission::EdgeCreate,
            Permission::EdgeRead,
            Permission::EdgeUpdate,
            Permission::EdgeDelete,
        ]
        .into_iter()
        .collect();
        roles.insert(
            Self::READWRITE_ROLE_ID,
            Role {
                id: Self::READWRITE_ROLE_ID,
                name: "readwrite".to_owned(),
                permissions: rw_perms,
                predefined: true,
            },
        );

        // Readonly — read nodes and edges
        let readonly_perms: HashSet<Permission> = [Permission::NodeRead, Permission::EdgeRead]
            .into_iter()
            .collect();
        roles.insert(
            Self::READONLY_ROLE_ID,
            Role {
                id: Self::READONLY_ROLE_ID,
                name: "readonly".to_owned(),
                permissions: readonly_perms,
                predefined: true,
            },
        );

        // Monitor — monitoring only
        let mon_perms: HashSet<Permission> = [Permission::Monitor, Permission::AdminAudit]
            .into_iter()
            .collect();
        roles.insert(
            Self::MONITOR_ROLE_ID,
            Role {
                id: Self::MONITOR_ROLE_ID,
                name: "monitor".to_owned(),
                permissions: mon_perms,
                predefined: true,
            },
        );

        Self {
            roles,
            next_id: Self::PREDEFINED_COUNT,
        }
    }

    /// Look up a role by ID.
    #[must_use]
    pub fn get(&self, id: RoleId) -> Option<&Role> {
        self.roles.get(&id)
    }

    /// Create a custom role with the given name and permissions.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::StorageError` if a role with the same name already exists.
    pub fn create_custom_role(
        &mut self,
        name: &str,
        permissions: HashSet<Permission>,
    ) -> Result<RoleId> {
        if self.roles.values().any(|r| r.name == name) {
            return Err(AuthError::StorageError(format!(
                "role already exists: {name}"
            )));
        }

        let id = RoleId(self.next_id);
        self.next_id += 1;

        self.roles.insert(
            id,
            Role {
                id,
                name: name.to_owned(),
                permissions,
                predefined: false,
            },
        );

        Ok(id)
    }

    /// Delete a custom role. Predefined roles cannot be deleted.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::PermissionDenied` if attempting to delete a predefined role,
    /// or `AuthError::StorageError` if the role does not exist.
    pub fn delete_role(&mut self, id: RoleId) -> Result<()> {
        let role = self
            .roles
            .get(&id)
            .ok_or_else(|| AuthError::StorageError(format!("role not found: {}", id.raw())))?;

        if role.predefined {
            return Err(AuthError::PermissionDenied {
                required: Permission::AdminRoles,
            });
        }

        self.roles.remove(&id);
        Ok(())
    }

    /// Collect the union of all permissions across the given role IDs.
    #[must_use]
    pub fn collect_permissions(&self, role_ids: &[RoleId]) -> HashSet<Permission> {
        let mut perms = HashSet::new();
        for &rid in role_ids {
            if let Some(role) = self.roles.get(&rid) {
                perms.extend(role.permissions.iter().copied());
            }
        }
        perms
    }
}

/// Thread-safe, cloneable handle to a `RoleStore`.
///
/// Wraps `Arc<RwLock<RoleStore>>` and exposes an ergonomic API that hides
/// lock management from callers.
#[derive(Clone)]
pub struct RoleStoreHandle {
    inner: Arc<RwLock<RoleStore>>,
}

impl RoleStoreHandle {
    /// Create a handle backed by a new `RoleStore` populated with predefined roles.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RoleStore::with_defaults())),
        }
    }

    /// Wrap an existing `Arc<RwLock<RoleStore>>`.
    #[must_use]
    pub const fn from_arc(arc: Arc<RwLock<RoleStore>>) -> Self {
        Self { inner: arc }
    }

    /// Look up a role by ID, returning a clone of it if found.
    ///
    /// Returns `None` if the role does not exist or if the lock is poisoned.
    #[must_use]
    pub fn get(&self, id: RoleId) -> Option<Role> {
        self.inner
            .read()
            .ok()
            .and_then(|store| store.roles.get(&id).cloned())
    }

    /// Collect the union of all permissions across the given role IDs.
    ///
    /// Returns an empty set if the lock is poisoned.
    #[must_use]
    pub fn collect_permissions(&self, role_ids: &[RoleId]) -> HashSet<Permission> {
        self.inner
            .read()
            .map(|store| store.collect_permissions(role_ids))
            .unwrap_or_default()
    }

    /// Create a custom role with the given name and permissions.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::LockPoisoned` if the internal lock is poisoned,
    /// or `AuthError::StorageError` if a role with the same name already exists.
    pub fn create_custom_role(
        &self,
        name: &str,
        permissions: HashSet<Permission>,
    ) -> Result<RoleId> {
        self.inner
            .write()
            .map_err(|_| AuthError::LockPoisoned("role store"))?
            .create_custom_role(name, permissions)
    }
}
