use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use aionui_api_types::{
    EnsureRenewableExternalSessionResponse, PublicUser, RefreshExternalSessionResponse,
    RenewableExternalSessionMetadata, RevokeMatchingExternalSessionResponse,
};
use aionui_db::models::User;
use aionui_db::{
    CoreAuthSessionError, CreateCoreAuthSessionParams, ICoreAuthSessionRepository, RotateAuthCredentialsParams,
    RotateCoreAuthSessionParams,
};

use crate::{AuthError, JwtService, TokenKind, TokenPayload, generate_random_secret_string};

type HmacSha256 = Hmac<Sha256>;

pub const DEFAULT_ACCESS_TTL: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_REFRESH_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub const JWT_SECRET_AUTHORITY_USER_ID: &str = "system_default_user";
const MIN_ACCESS_TTL: Duration = Duration::from_secs(60);
const MAX_ACCESS_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_REFRESH_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLifecycleConfig {
    pub access_ttl: Duration,
    pub refresh_ttl: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionLifecycleConfigError {
    #[error("external session ttl must be an unsigned integer number of seconds")]
    TtlNotInteger,
    #[error("external access session ttl must be between 60 and 3600 seconds")]
    AccessTtlInvalid,
    #[error("external refresh session ttl must exceed access ttl and be at most 90 days")]
    RefreshTtlInvalid,
}

impl SessionLifecycleConfig {
    pub fn new(access_ttl: Duration, refresh_ttl: Duration) -> Result<Self, SessionLifecycleConfigError> {
        if !(MIN_ACCESS_TTL..=MAX_ACCESS_TTL).contains(&access_ttl) {
            return Err(SessionLifecycleConfigError::AccessTtlInvalid);
        }
        if refresh_ttl <= access_ttl || refresh_ttl > MAX_REFRESH_TTL {
            return Err(SessionLifecycleConfigError::RefreshTtlInvalid);
        }
        Ok(Self {
            access_ttl,
            refresh_ttl,
        })
    }

    pub fn from_values(
        access_ttl: Option<&str>,
        refresh_ttl: Option<&str>,
    ) -> Result<Self, SessionLifecycleConfigError> {
        let parse = |value: Option<&str>, default: Duration| {
            value
                .map(|value| value.parse::<u64>().map(Duration::from_secs))
                .transpose()
                .map(|value| value.unwrap_or(default))
                .map_err(|_| SessionLifecycleConfigError::TtlNotInteger)
        };
        Self::new(
            parse(access_ttl, DEFAULT_ACCESS_TTL)?,
            parse(refresh_ttl, DEFAULT_REFRESH_TTL)?,
        )
    }
}

impl Default for SessionLifecycleConfig {
    fn default() -> Self {
        Self {
            access_ttl: DEFAULT_ACCESS_TTL,
            refresh_ttl: DEFAULT_REFRESH_TTL,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionLifecycleError {
    #[error("refresh credential is required")]
    RefreshRequired,
    #[error("refresh credential is invalid")]
    RefreshInvalid,
    #[error("refresh idempotency key is required")]
    IdempotencyKeyRequired,
    #[error("refresh idempotency key is invalid")]
    IdempotencyKeyInvalid,
    #[error("refresh credential was replayed")]
    RefreshReplayed,
    #[error("external session expired")]
    Expired,
    #[error("external session revoked")]
    Revoked,
    #[error("Core user is disabled")]
    UserDisabled,
    #[error("external session generation is stale")]
    GenerationMismatch,
    #[error("JWT secret is managed by the environment")]
    EnvironmentManagedSecret,
    #[error("external session unavailable")]
    Database(#[source] aionui_db::DbError),
    #[error("external session signing failed")]
    Token(#[source] AuthError),
    #[error("persisted JWT secret could not be activated; authentication cannot continue")]
    RuntimeActivation(#[source] AuthError),
    #[error("system clock unavailable")]
    Clock,
}

pub struct RenewableSessionExchange {
    pub response: EnsureRenewableExternalSessionResponse,
    pub access_token: String,
    pub refresh_credential: String,
}

pub struct RefreshSessionExchange {
    pub response: RefreshExternalSessionResponse,
    pub access_token: String,
    pub refresh_credential: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAccessSession {
    pub sid: String,
    pub user_id: String,
    pub username: String,
    pub rotation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtSecretSource {
    Database,
    Environment,
}

#[derive(Clone)]
pub struct SessionLifecycle {
    repository: Arc<dyn ICoreAuthSessionRepository>,
    jwt_service: Arc<JwtService>,
    config: SessionLifecycleConfig,
    refresh_key: Arc<tokio::sync::RwLock<[u8; 32]>>,
    jwt_secret_source: JwtSecretSource,
    jwt_secret_user_id: Arc<str>,
}

impl SessionLifecycle {
    pub fn new(
        repository: Arc<dyn ICoreAuthSessionRepository>,
        jwt_service: Arc<JwtService>,
        config: SessionLifecycleConfig,
        refresh_key: [u8; 32],
    ) -> Self {
        Self::new_with_source(
            repository,
            jwt_service,
            config,
            refresh_key,
            JwtSecretSource::Database,
            JWT_SECRET_AUTHORITY_USER_ID,
        )
    }

    pub fn new_with_source(
        repository: Arc<dyn ICoreAuthSessionRepository>,
        jwt_service: Arc<JwtService>,
        config: SessionLifecycleConfig,
        refresh_key: [u8; 32],
        jwt_secret_source: JwtSecretSource,
        jwt_secret_user_id: &str,
    ) -> Self {
        Self {
            repository,
            jwt_service,
            config,
            refresh_key: Arc::new(tokio::sync::RwLock::new(refresh_key)),
            jwt_secret_source,
            jwt_secret_user_id: Arc::from(jwt_secret_user_id),
        }
    }

    pub fn config(&self) -> SessionLifecycleConfig {
        self.config
    }

    pub async fn issue(&self, user: User) -> Result<RenewableSessionExchange, SessionLifecycleError> {
        let refresh_key = self.refresh_key.read().await;
        let now = now_ms()?;
        let sid = aionui_common::generate_prefixed_id("core_session");
        let secret = derive_refresh_secret(&refresh_key, &sid, 0);
        let secret_hash = hash_value(&secret);
        let session_expires_at = add_duration_ms(now, self.config.refresh_ttl)?;
        let session = self
            .repository
            .create(CreateCoreAuthSessionParams {
                sid: &sid,
                user_id: &user.id,
                current_refresh_hash: &secret_hash,
                session_generation: user.session_generation,
                session_expires_at,
                now,
            })
            .await
            .map_err(map_repository_error)?;
        let username = user.username.unwrap_or_else(|| "external_user".to_owned());
        let signed = match self.jwt_service.sign_external_access(
            &session.user_id,
            &username,
            session.session_generation,
            &session.sid,
            session.rotation,
            self.config.access_ttl,
        ) {
            Ok(signed) => signed,
            Err(error) => {
                let _ = self.repository.revoke_matching(&sid, &secret_hash, now).await;
                return Err(SessionLifecycleError::Token(error));
            }
        };
        let metadata = metadata(&session, signed.expires_at);
        Ok(RenewableSessionExchange {
            response: EnsureRenewableExternalSessionResponse {
                user: PublicUser {
                    id: session.user_id,
                    username,
                },
                session_generation: session.session_generation,
                session: metadata,
            },
            access_token: signed.token,
            refresh_credential: format!("{}.{}", session.sid, secret),
        })
    }

    pub async fn refresh(
        &self,
        credential: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<RefreshSessionExchange, SessionLifecycleError> {
        let (sid, presented_secret) = parse_refresh_credential(credential)?;
        // Validate the retry key before touching persistence. Missing or
        // malformed keys are caller errors and must never mutate session state.
        let key_hash = hash_value(parse_idempotency_key(idempotency_key)?);
        let refresh_key = self.refresh_key.read().await;
        let before = self
            .repository
            .find(sid)
            .await
            .map_err(SessionLifecycleError::Database)?
            .ok_or(SessionLifecycleError::RefreshInvalid)?;
        let replacement_secret = derive_refresh_secret(&refresh_key, sid, before.rotation.saturating_add(1));
        let presented_hash = hash_value(presented_secret);
        let replacement_hash = hash_value(&replacement_secret);
        let now = now_ms()?;
        let rotated = self
            .repository
            .rotate(RotateCoreAuthSessionParams {
                sid,
                presented_secret_hash: &presented_hash,
                replacement_secret_hash: &replacement_hash,
                rotation_key_hash: &key_hash,
                now,
            })
            .await
            .map_err(map_repository_error)?;
        let current_secret = derive_refresh_secret(&refresh_key, &rotated.session.sid, rotated.session.rotation);
        let signed = match self.jwt_service.sign_external_access(
            &rotated.session.user_id,
            &rotated.username,
            rotated.session.session_generation,
            &rotated.session.sid,
            rotated.session.rotation,
            self.config.access_ttl,
        ) {
            Ok(signed) => signed,
            Err(error) => {
                let _ = self
                    .repository
                    .revoke_matching(&rotated.session.sid, &hash_value(&current_secret), now)
                    .await;
                return Err(SessionLifecycleError::Token(error));
            }
        };
        Ok(RefreshSessionExchange {
            response: RefreshExternalSessionResponse {
                session: metadata(&rotated.session, signed.expires_at),
            },
            access_token: signed.token,
            refresh_credential: format!("{}.{}", rotated.session.sid, current_secret),
        })
    }

    pub async fn revoke_matching(
        &self,
        credential: Option<&str>,
    ) -> Result<RevokeMatchingExternalSessionResponse, SessionLifecycleError> {
        let (sid, secret) = parse_refresh_credential(credential)?;
        let session = self
            .repository
            .revoke_matching(sid, &hash_value(secret), now_ms()?)
            .await
            .map_err(map_repository_error)?;
        Ok(RevokeMatchingExternalSessionResponse {
            sid: session.sid,
            revoked: true,
        })
    }

    pub async fn revoke_user(&self, user_id: &str) -> Result<i64, SessionLifecycleError> {
        self.repository
            .revoke_user(user_id, now_ms()?)
            .await
            .map_err(map_repository_error)
    }

    pub fn ensure_master_secret_rotation_allowed(&self) -> Result<(), SessionLifecycleError> {
        if self.jwt_secret_source == JwtSecretSource::Environment {
            return Err(SessionLifecycleError::EnvironmentManagedSecret);
        }
        Ok(())
    }

    pub async fn rotate_auth_credentials(
        &self,
        user_id: &str,
        expected_password_hash: &str,
        new_password_hash: &str,
    ) -> Result<(), SessionLifecycleError> {
        self.ensure_master_secret_rotation_allowed()?;
        let mut refresh_key = self.refresh_key.write().await;
        let new_secret = generate_random_secret_string();
        self.repository
            .rotate_auth_credentials(RotateAuthCredentialsParams {
                password_user_id: user_id,
                jwt_secret_user_id: &self.jwt_secret_user_id,
                expected_password_hash,
                new_password_hash,
                new_jwt_secret: &new_secret,
                now: now_ms()?,
            })
            .await
            .map_err(SessionLifecycleError::Database)?;
        if let Err(error) = self.jwt_service.activate_secret(new_secret.clone()) {
            tracing::error!(
                "fatal JWT runtime activation failure after committed credential rotation; restart required"
            );
            return Err(SessionLifecycleError::RuntimeActivation(error));
        }
        *refresh_key = derive_refresh_key(&new_secret);
        Ok(())
    }

    pub async fn verify_access(
        &self,
        payload: &TokenPayload,
    ) -> Result<Option<VerifiedAccessSession>, SessionLifecycleError> {
        if payload.token_kind.is_none() && payload.sid.is_none() && payload.session_rotation.is_none() {
            return Ok(None);
        }
        if payload.token_kind != Some(TokenKind::Access) {
            return Err(SessionLifecycleError::RefreshInvalid);
        }
        let sid = payload.sid.as_deref().ok_or(SessionLifecycleError::RefreshInvalid)?;
        let rotation = payload.session_rotation.ok_or(SessionLifecycleError::RefreshInvalid)?;
        let active = self
            .repository
            .validate_access(sid, &payload.user_id, payload.session_generation, rotation, now_ms()?)
            .await
            .map_err(map_repository_error)?;
        Ok(Some(VerifiedAccessSession {
            sid: active.session.sid,
            user_id: active.session.user_id,
            username: active.username,
            rotation: active.session.rotation,
        }))
    }

    pub async fn prune_terminal(&self) -> Result<u64, SessionLifecycleError> {
        self.repository
            .prune_terminal(now_ms()?)
            .await
            .map_err(SessionLifecycleError::Database)
    }
}

fn derive_refresh_secret(refresh_key: &[u8; 32], sid: &str, rotation: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(refresh_key).expect("HMAC accepts a 32-byte key");
    mac.update(b"aioncore-refresh-secret-v1\0");
    mac.update(sid.as_bytes());
    mac.update(b"\0");
    mac.update(&rotation.to_be_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub fn derive_refresh_key(jwt_secret: &str) -> [u8; 32] {
    // Deliberately derived from the JWT master secret with a separate domain:
    // rotating that master secret invalidates both access and durable refresh
    // credentials instead of leaving a second long-lived secret behind.
    let mut hasher = Sha256::new();
    hasher.update(b"aioncore-refresh-key-v1\0");
    hasher.update(jwt_secret.as_bytes());
    hasher.finalize().into()
}

fn metadata(session: &aionui_db::CoreAuthSession, access_expires_at: u64) -> RenewableExternalSessionMetadata {
    RenewableExternalSessionMetadata {
        sid: session.sid.clone(),
        rotation: session.rotation,
        access_expires_at,
        refresh_expires_at: u64::try_from(session.session_expires_at / 1000).unwrap_or(0),
    }
}

fn parse_refresh_credential(credential: Option<&str>) -> Result<(&str, &str), SessionLifecycleError> {
    let credential = credential
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SessionLifecycleError::RefreshRequired)?;
    let (sid, secret) = credential
        .split_once('.')
        .filter(|(sid, secret)| !sid.is_empty() && !secret.is_empty() && !secret.contains('.'))
        .ok_or(SessionLifecycleError::RefreshInvalid)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(secret)
        .map_err(|_| SessionLifecycleError::RefreshInvalid)?;
    if decoded.len() != 32 {
        return Err(SessionLifecycleError::RefreshInvalid);
    }
    Ok((sid, secret))
}

fn parse_idempotency_key(value: Option<&str>) -> Result<&str, SessionLifecycleError> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SessionLifecycleError::IdempotencyKeyRequired)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| SessionLifecycleError::IdempotencyKeyInvalid)?;
    if decoded.len() != 32 {
        return Err(SessionLifecycleError::IdempotencyKeyInvalid);
    }
    Ok(value)
}

fn hash_value(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let mut output = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn now_ms() -> Result<i64, SessionLifecycleError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SessionLifecycleError::Clock)
        .and_then(|duration| i64::try_from(duration.as_millis()).map_err(|_| SessionLifecycleError::Clock))
}

fn add_duration_ms(now: i64, duration: Duration) -> Result<i64, SessionLifecycleError> {
    let millis = i64::try_from(duration.as_millis()).map_err(|_| SessionLifecycleError::Clock)?;
    now.checked_add(millis).ok_or(SessionLifecycleError::Clock)
}

fn map_repository_error(error: CoreAuthSessionError) -> SessionLifecycleError {
    match error {
        CoreAuthSessionError::NotFound | CoreAuthSessionError::CrossUser => SessionLifecycleError::RefreshInvalid,
        CoreAuthSessionError::Replay => SessionLifecycleError::RefreshReplayed,
        CoreAuthSessionError::Expired => SessionLifecycleError::Expired,
        CoreAuthSessionError::Revoked => SessionLifecycleError::Revoked,
        CoreAuthSessionError::UserDisabled => SessionLifecycleError::UserDisabled,
        CoreAuthSessionError::GenerationMismatch => SessionLifecycleError::GenerationMismatch,
        CoreAuthSessionError::Database(error) => SessionLifecycleError::Database(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aionui_db::{
        ActiveCoreAuthSession, CoreAuthSession, DbError, RotateCoreAuthSessionResult, SqliteCoreAuthSessionRepository,
        init_database_memory,
    };
    use tokio::sync::Barrier;

    struct BlockingCreateRepository {
        inner: Arc<dyn ICoreAuthSessionRepository>,
        create_entered: Arc<Barrier>,
        create_release: Arc<Barrier>,
    }

    #[async_trait::async_trait]
    impl ICoreAuthSessionRepository for BlockingCreateRepository {
        async fn find(&self, sid: &str) -> Result<Option<CoreAuthSession>, DbError> {
            self.inner.find(sid).await
        }

        async fn create(
            &self,
            params: CreateCoreAuthSessionParams<'_>,
        ) -> Result<CoreAuthSession, CoreAuthSessionError> {
            self.create_entered.wait().await;
            self.create_release.wait().await;
            self.inner.create(params).await
        }

        async fn rotate(
            &self,
            params: RotateCoreAuthSessionParams<'_>,
        ) -> Result<RotateCoreAuthSessionResult, CoreAuthSessionError> {
            self.inner.rotate(params).await
        }

        async fn validate_access(
            &self,
            sid: &str,
            user_id: &str,
            session_generation: i64,
            rotation: i64,
            now: i64,
        ) -> Result<ActiveCoreAuthSession, CoreAuthSessionError> {
            self.inner
                .validate_access(sid, user_id, session_generation, rotation, now)
                .await
        }

        async fn revoke_matching(
            &self,
            sid: &str,
            presented_secret_hash: &str,
            now: i64,
        ) -> Result<CoreAuthSession, CoreAuthSessionError> {
            self.inner.revoke_matching(sid, presented_secret_hash, now).await
        }

        async fn revoke_user(&self, user_id: &str, now: i64) -> Result<i64, CoreAuthSessionError> {
            self.inner.revoke_user(user_id, now).await
        }

        async fn rotate_auth_credentials(&self, params: RotateAuthCredentialsParams<'_>) -> Result<u64, DbError> {
            self.inner.rotate_auth_credentials(params).await
        }

        async fn prune_terminal(&self, now: i64) -> Result<u64, DbError> {
            self.inner.prune_terminal(now).await
        }
    }

    #[test]
    fn external_session_ttls_are_independent_and_fail_closed() {
        assert_eq!(
            SessionLifecycleConfig::from_values(None, None).unwrap(),
            SessionLifecycleConfig::default()
        );
        assert!(matches!(
            SessionLifecycleConfig::from_values(Some("59"), None),
            Err(SessionLifecycleConfigError::AccessTtlInvalid)
        ));
        assert!(matches!(
            SessionLifecycleConfig::from_values(Some("900"), Some("900")),
            Err(SessionLifecycleConfigError::RefreshTtlInvalid)
        ));
        assert!(matches!(
            SessionLifecycleConfig::from_values(Some("not-a-number"), None),
            Err(SessionLifecycleConfigError::TtlNotInteger)
        ));
    }

    #[test]
    fn refresh_key_is_domain_separated_and_rotates_with_jwt_master_secret() {
        let first = derive_refresh_key("jwt-master-a");
        assert_eq!(first, derive_refresh_key("jwt-master-a"));
        assert_ne!(first, derive_refresh_key("jwt-master-b"));
        let raw_hash: [u8; 32] = Sha256::digest(b"jwt-master-a").into();
        assert_ne!(first, raw_hash);
    }

    #[test]
    fn idempotency_key_requires_exactly_32_base64url_bytes() {
        assert!(matches!(
            parse_idempotency_key(None),
            Err(SessionLifecycleError::IdempotencyKeyRequired)
        ));
        assert!(matches!(
            parse_idempotency_key(Some("malformed")),
            Err(SessionLifecycleError::IdempotencyKeyInvalid)
        ));
        assert!(parse_idempotency_key(Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")).is_ok());
    }

    #[tokio::test]
    async fn master_rotation_waits_for_in_flight_issue_then_revokes_its_session() {
        let db = init_database_memory().await.unwrap();
        sqlx::query("UPDATE users SET password_hash = 'old-hash' WHERE id = ?")
            .bind(JWT_SECRET_AUTHORITY_USER_ID)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, status, session_generation, created_at, updated_at) \
             VALUES ('external-race', 'external', 'external-race', 'active', 0, 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = 'external-race'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let create_entered = Arc::new(Barrier::new(2));
        let create_release = Arc::new(Barrier::new(2));
        let repository = Arc::new(BlockingCreateRepository {
            inner: Arc::new(SqliteCoreAuthSessionRepository::new(db.pool().clone())),
            create_entered: create_entered.clone(),
            create_release: create_release.clone(),
        });
        let jwt_service = Arc::new(JwtService::new("master-before".into()));
        let lifecycle = Arc::new(SessionLifecycle::new(
            repository,
            jwt_service.clone(),
            SessionLifecycleConfig::default(),
            derive_refresh_key("master-before"),
        ));

        let issuing = {
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move { lifecycle.issue(user).await.unwrap() })
        };
        create_entered.wait().await;

        let rotation_started = Arc::new(Barrier::new(2));
        let rotating = {
            let lifecycle = lifecycle.clone();
            let rotation_started = rotation_started.clone();
            tokio::spawn(async move {
                rotation_started.wait().await;
                lifecycle
                    .rotate_auth_credentials(JWT_SECRET_AUTHORITY_USER_ID, "old-hash", "new-hash")
                    .await
                    .unwrap()
            })
        };
        rotation_started.wait().await;
        tokio::task::yield_now().await;
        assert!(
            !rotating.is_finished(),
            "rotation must wait for the in-flight issue read lock"
        );

        create_release.wait().await;
        let issued = issuing.await.unwrap();
        rotating.await.unwrap();
        let new_secret: String = sqlx::query_scalar("SELECT jwt_secret FROM users WHERE id = ?")
            .bind(JWT_SECRET_AUTHORITY_USER_ID)
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(jwt_service.verify(&issued.access_token).is_err());
        assert!(JwtService::new(new_secret).verify(&issued.access_token).is_err());
        assert!(matches!(
            lifecycle
                .refresh(
                    Some(&issued.refresh_credential),
                    Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                )
                .await,
            Err(SessionLifecycleError::Revoked)
        ));
    }
}
