// Copyright 2026 BelowZero Security OU. All rights reserved.

use crate::rbac::Permission;

/// Central error type for all authentication and authorization operations.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid credentials")]
    InvalidCredentials,

    #[error("user not found: {0}")]
    UserNotFound(String),

    #[error("user already exists: {0}")]
    UserAlreadyExists(String),

    #[error("permission denied: requires {required}")]
    PermissionDenied { required: Permission },

    #[error("session token expired")]
    TokenExpired,

    #[error("invalid session token")]
    TokenInvalid,

    #[error("password policy violation: {0}")]
    PasswordPolicyViolation(String),

    #[error("storage error: {0}")]
    StorageError(String),

    #[error("lock poisoned: {0}")]
    LockPoisoned(&'static str),

    #[error("account locked: too many failed attempts")]
    AccountLocked,
}

/// Convenience result type for auth operations.
pub type Result<T> = std::result::Result<T, AuthError>;
