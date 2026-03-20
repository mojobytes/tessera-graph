// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use zeroize::Zeroize;

use crate::credentials::{Password, PasswordHash, PasswordHasher, PasswordPolicy};
use crate::error::{AuthError, Result};
use crate::rate_limit::{LoginAttemptTracker, LoginPolicy};
use crate::rbac::RoleId;
use crate::utils::unix_timestamp;

/// Unique identifier for a user in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct UserId(u64);

impl UserId {
    /// Create a `UserId` from a raw value.
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

/// Internal record for a single user. Serialized to JSON for persistence.
///
/// Does not implement `Clone` — the `password_hash` field is zeroized on drop
/// to limit the lifetime of the credential in memory.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct UserRecord {
    pub id: UserId,
    pub username: String,
    pub password_hash: String,
    pub roles: Vec<RoleId>,
    pub created_at: u64,
    pub password_changed_at: u64,
}

impl Zeroize for UserRecord {
    fn zeroize(&mut self) {
        self.password_hash.zeroize();
    }
}

impl Drop for UserRecord {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// In-memory user store. Not exposed directly — access through `UserStoreHandle`.
#[derive(serde::Serialize, serde::Deserialize)]
struct UserStore {
    users: HashMap<String, UserRecord>,
    next_id: u64,
    /// Secondary index: user ID → username for O(1) reverse lookup.
    /// Not serialized — rebuilt from `users` on deserialization.
    #[serde(skip)]
    id_to_username: HashMap<UserId, String>,
}

/// Thread-safe handle to the user store.
#[derive(Clone)]
pub struct UserStoreHandle {
    inner: Arc<RwLock<UserStore>>,
    hasher: Arc<PasswordHasher>,
}

impl UserStoreHandle {
    /// Create a new store with a built-in admin user (id = 0).
    ///
    /// # Errors
    ///
    /// Returns an error if password hashing fails.
    pub fn new(
        admin_username: &str,
        admin_password: &Password,
        policy: &PasswordPolicy,
    ) -> Result<Self> {
        policy.validate_raw_str(admin_password.as_str())?;
        let hasher = PasswordHasher::new();
        let hash = hasher.hash(admin_password)?;
        let now = unix_timestamp();

        let admin_record = UserRecord {
            id: UserId(0),
            username: admin_username.to_owned(),
            password_hash: hash.as_str().to_owned(),
            roles: vec![],
            created_at: now,
            password_changed_at: now,
        };

        let mut users = HashMap::new();
        users.insert(admin_username.to_owned(), admin_record);

        let mut id_to_username = HashMap::new();
        id_to_username.insert(UserId(0), admin_username.to_owned());

        let store = UserStore {
            users,
            next_id: 1,
            id_to_username,
        };

        Ok(Self {
            inner: Arc::new(RwLock::new(store)),
            hasher: Arc::new(hasher),
        })
    }

    /// Create a new user in the store.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::UserAlreadyExists` if the username is taken,
    /// or a hashing error on failure.
    pub fn create_user(
        &self,
        username: &str,
        password: &Password,
        roles: Vec<RoleId>,
        policy: &PasswordPolicy,
    ) -> Result<UserId> {
        policy.validate_raw_str(password.as_str())?;
        let hash = self.hasher.hash(password)?;
        let now = unix_timestamp();

        let mut store = self
            .inner
            .write()
            .map_err(|_| AuthError::LockPoisoned("user store"))?;

        if store.users.contains_key(username) {
            return Err(AuthError::UserAlreadyExists(username.to_owned()));
        }

        let id = UserId(store.next_id);
        store.next_id += 1;

        let record = UserRecord {
            id,
            username: username.to_owned(),
            password_hash: hash.as_str().to_owned(),
            roles,
            created_at: now,
            password_changed_at: now,
        };

        store.users.insert(username.to_owned(), record);
        store.id_to_username.insert(id, username.to_owned());
        drop(store);
        Ok(id)
    }

    /// Authenticate a user by username and password.
    ///
    /// Returns the same error variant for both non-existent user and wrong password
    /// to prevent user enumeration.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::InvalidCredentials` on authentication failure.
    pub fn authenticate(&self, username: &str, password: &Password) -> Result<UserId> {
        // Dummy hash used for timing-safe comparison when user does not exist.
        let dummy_hash = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

        let (hash_str, user_id) = {
            let store = self
                .inner
                .read()
                .map_err(|_| AuthError::LockPoisoned("user store"))?;

            store.users.get(username).map_or_else(
                || (dummy_hash.to_owned(), None),
                |record| (record.password_hash.clone(), Some(record.id)),
            )
        };

        let stored_hash = PasswordHash::from_stored(hash_str);
        let verify_result = self.hasher.verify(password, &stored_hash);

        match (verify_result, user_id) {
            (Ok(()), Some(id)) => Ok(id),
            _ => Err(AuthError::InvalidCredentials),
        }
    }

    /// Delete a user from the store.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::UserNotFound` if the user does not exist.
    pub fn delete_user(&self, username: &str) -> Result<()> {
        let mut store = self
            .inner
            .write()
            .map_err(|_| AuthError::LockPoisoned("user store"))?;

        let removed = store
            .users
            .remove(username)
            .ok_or_else(|| AuthError::UserNotFound(username.to_owned()))?;
        store.id_to_username.remove(&removed.id);
        drop(store);
        Ok(())
    }

    /// Change a user's password after verifying the old one.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::InvalidCredentials` if old password is wrong,
    /// or policy/hashing errors.
    pub fn change_password(
        &self,
        username: &str,
        old_password: &Password,
        new_password: &Password,
        policy: &PasswordPolicy,
    ) -> Result<()> {
        policy.validate_raw_str(new_password.as_str())?;

        // 1. Read lock only — extract the stored hash, then release
        let stored_hash_str = self
            .inner
            .read()
            .map_err(|_| AuthError::LockPoisoned("user store"))?
            .users
            .get(username)
            .ok_or_else(|| AuthError::UserNotFound(username.to_owned()))?
            .password_hash
            .clone();
        let stored_hash = PasswordHash::from_stored(stored_hash_str);

        // 2. Argon2 verify + hash OUTSIDE the lock (~200ms)
        self.hasher.verify(old_password, &stored_hash)?;
        let new_hash = self.hasher.hash(new_password)?;

        // 3. Write lock only to update the record.
        //    Re-check that the stored hash has not changed between step 1 and now
        //    (TOCTOU guard). If another concurrent change_password succeeded in
        //    the window between our read-lock and this write-lock, the hashes will
        //    differ and we reject to avoid silently overwriting the new credential.
        let mut store = self
            .inner
            .write()
            .map_err(|_| AuthError::LockPoisoned("user store"))?;
        let record = store
            .users
            .get_mut(username)
            .ok_or_else(|| AuthError::UserNotFound(username.to_owned()))?;
        if record.password_hash != stored_hash.as_str() {
            return Err(AuthError::InvalidCredentials);
        }
        new_hash.as_str().clone_into(&mut record.password_hash);
        record.password_changed_at = unix_timestamp();
        drop(store);

        Ok(())
    }

    /// List all usernames in the store. Does not expose hashes or other sensitive data.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::LockPoisoned` if the internal lock is poisoned.
    pub fn list_usernames(&self) -> Result<Vec<String>> {
        let store = self
            .inner
            .read()
            .map_err(|_| AuthError::LockPoisoned("user store"))?;
        Ok(store.users.keys().cloned().collect())
    }

    /// Get the roles assigned to a user.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::UserNotFound` if the user does not exist.
    pub fn get_user_roles(&self, user_id: UserId) -> Result<Vec<RoleId>> {
        let store = self
            .inner
            .read()
            .map_err(|_| AuthError::LockPoisoned("user store"))?;

        let username = store
            .id_to_username
            .get(&user_id)
            .ok_or_else(|| AuthError::UserNotFound(format!("id={}", user_id.raw())))?;

        let roles = store
            .users
            .get(username)
            .map(|r| r.roles.clone())
            .ok_or_else(|| AuthError::UserNotFound(format!("id={}", user_id.raw())))?;

        drop(store);
        Ok(roles)
    }

    /// Assign a role to a user.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::UserNotFound` if the user does not exist.
    pub fn assign_role(&self, username: &str, role_id: RoleId) -> Result<()> {
        let mut store = self
            .inner
            .write()
            .map_err(|_| AuthError::LockPoisoned("user store"))?;

        let record = store
            .users
            .get_mut(username)
            .ok_or_else(|| AuthError::UserNotFound(username.to_owned()))?;

        if !record.roles.contains(&role_id) {
            record.roles.push(role_id);
        }
        drop(store);

        Ok(())
    }

    /// Authenticate with brute-force protection.
    ///
    /// Checks the rate limiter before attempting authentication. On success,
    /// resets the failure counter. On failure, records the attempt.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::AccountLocked` if the account is locked,
    /// or `AuthError::InvalidCredentials` on authentication failure.
    pub fn authenticate_with_rate_limit(
        &self,
        username: &str,
        password: &Password,
        tracker: &LoginAttemptTracker,
        login_policy: &LoginPolicy,
    ) -> Result<UserId> {
        if tracker.is_locked(username, login_policy) {
            return Err(AuthError::AccountLocked);
        }

        match self.authenticate(username, password) {
            Ok(user_id) => {
                tracker.record_success(username);
                Ok(user_id)
            }
            Err(e) => {
                tracker.record_failure(username);
                Err(e)
            }
        }
    }

    /// Persist the store to a JSON file using an atomic write.
    ///
    /// The data is first written to `<path>.tmp`, then renamed to `path`.
    /// On POSIX systems `rename(2)` is atomic, so readers never observe a
    /// partially-written file.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::StorageError` on I/O or serialization failure.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let json = {
            let store = self
                .inner
                .read()
                .map_err(|_| AuthError::LockPoisoned("user store"))?;

            serde_json::to_string_pretty(&*store)
                .map_err(|e| AuthError::StorageError(format!("serialization failed: {e}")))?
        };

        let mut tmp_path = path.as_os_str().to_owned();
        tmp_path.push(".tmp");
        let tmp_path = std::path::PathBuf::from(tmp_path);

        std::fs::write(&tmp_path, json)
            .map_err(|e| AuthError::StorageError(format!("write failed: {e}")))?;

        std::fs::rename(&tmp_path, path)
            .map_err(|e| AuthError::StorageError(format!("atomic rename failed: {e}")))?;

        Ok(())
    }

    /// Load a store from a JSON file.
    ///
    /// # Errors
    ///
    /// Returns `AuthError::StorageError` on I/O or deserialization failure.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .map_err(|e| AuthError::StorageError(format!("read failed: {e}")))?;

        let mut store: UserStore = serde_json::from_str(&data)
            .map_err(|e| AuthError::StorageError(format!("deserialization failed: {e}")))?;

        // Rebuild the secondary index from the deserialized user records.
        store.id_to_username = store
            .users
            .values()
            .map(|r| (r.id, r.username.clone()))
            .collect();

        Ok(Self {
            inner: Arc::new(RwLock::new(store)),
            hasher: Arc::new(PasswordHasher::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::UserRecord;

    #[test]
    fn user_record_implements_zeroize() {
        fn assert_zeroize<T: zeroize::Zeroize>() {}
        assert_zeroize::<UserRecord>();
    }
}
