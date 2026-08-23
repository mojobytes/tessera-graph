// SPDX-License-Identifier: BSL-1.1

//! No-op provider for tests and opt-in dev mode.

use async_trait::async_trait;

use super::{AuthError, AuthOutcome, AuthProvider};

/// Accepts every credential with `user_id = "anonymous"`.
///
/// Startup must only select this provider when `ERMYA_NO_AUTH=1` is
/// explicitly set (see Task 9). Tests construct it directly.
pub struct NoAuthProvider;

#[async_trait]
impl AuthProvider for NoAuthProvider {
    async fn authenticate(
        &self,
        _principal: &str,
        _credentials: &str,
    ) -> Result<AuthOutcome, AuthError> {
        Ok(AuthOutcome {
            user_id: "anonymous".to_owned(),
            roles: Vec::new(),
            is_admin: false,
        })
    }
}
