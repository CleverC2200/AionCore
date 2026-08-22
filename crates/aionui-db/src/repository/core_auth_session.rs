use aionui_common::TimestampMs;

use crate::{DbError, models::CoreAuthSession};

#[derive(Debug, Clone)]
pub struct CreateCoreAuthSessionParams<'a> {
    pub sid: &'a str,
    pub user_id: &'a str,
    pub current_refresh_hash: &'a str,
    pub session_generation: i64,
    pub session_expires_at: TimestampMs,
    pub now: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct RotateCoreAuthSessionParams<'a> {
    pub sid: &'a str,
    pub presented_secret_hash: &'a str,
    pub replacement_secret_hash: &'a str,
    pub rotation_key_hash: &'a str,
    pub now: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct RotateAuthCredentialsParams<'a> {
    pub user_id: &'a str,
    pub expected_password_hash: &'a str,
    pub new_password_hash: &'a str,
    pub new_jwt_secret: &'a str,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateCoreAuthSessionResult {
    pub session: CoreAuthSession,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCoreAuthSession {
    pub session: CoreAuthSession,
    pub username: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreAuthSessionError {
    #[error("renewable session not found")]
    NotFound,
    #[error("renewable session refresh credential was replayed")]
    Replay,
    #[error("renewable session expired")]
    Expired,
    #[error("renewable session revoked")]
    Revoked,
    #[error("renewable session user is disabled")]
    UserDisabled,
    #[error("renewable session generation is stale")]
    GenerationMismatch,
    #[error("renewable session belongs to another user")]
    CrossUser,
    #[error("renewable session persistence failed")]
    Database(#[from] DbError),
}

#[async_trait::async_trait]
pub trait ICoreAuthSessionRepository: Send + Sync {
    async fn find(&self, sid: &str) -> Result<Option<CoreAuthSession>, DbError>;

    async fn create(&self, params: CreateCoreAuthSessionParams<'_>) -> Result<CoreAuthSession, CoreAuthSessionError>;

    /// Atomically rotates the refresh secret. A hash mismatch is treated as
    /// replay and revokes the entire sid before returning.
    async fn rotate(
        &self,
        params: RotateCoreAuthSessionParams<'_>,
    ) -> Result<RotateCoreAuthSessionResult, CoreAuthSessionError>;

    async fn validate_access(
        &self,
        sid: &str,
        user_id: &str,
        session_generation: i64,
        rotation: i64,
        now: TimestampMs,
    ) -> Result<ActiveCoreAuthSession, CoreAuthSessionError>;

    async fn revoke_matching(
        &self,
        sid: &str,
        presented_secret_hash: &str,
        now: TimestampMs,
    ) -> Result<CoreAuthSession, CoreAuthSessionError>;

    /// User-wide administrative revocation: increments the user's generation
    /// and revokes all renewable rows in the same transaction.
    async fn revoke_user(&self, user_id: &str, now: TimestampMs) -> Result<i64, CoreAuthSessionError>;

    /// Atomically replace a verified local user's password and persisted JWT
    /// secret while revoking every durable external session.
    async fn rotate_auth_credentials(&self, params: RotateAuthCredentialsParams<'_>) -> Result<u64, DbError>;

    /// Delete terminal rows during startup. The durable expiry is absolute,
    /// so pruning cannot extend or otherwise mutate a live session.
    async fn prune_terminal(&self, now: TimestampMs) -> Result<u64, DbError>;
}
