use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::DbError;
use crate::models::{CoreAuthSession, User, UserStatus, UserType};
use crate::repository::core_auth_session::{
    ActiveCoreAuthSession, CoreAuthSessionError, CreateCoreAuthSessionParams, ICoreAuthSessionRepository,
    RotateAuthCredentialsParams, RotateCoreAuthSessionParams, RotateCoreAuthSessionResult,
};

#[derive(Clone, Debug)]
pub struct SqliteCoreAuthSessionRepository {
    pool: SqlitePool,
}

impl SqliteCoreAuthSessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ICoreAuthSessionRepository for SqliteCoreAuthSessionRepository {
    async fn find(&self, sid: &str) -> Result<Option<CoreAuthSession>, DbError> {
        sqlx::query_as::<_, CoreAuthSession>("SELECT * FROM core_auth_sessions WHERE sid = ?")
            .bind(sid)
            .fetch_optional(&self.pool)
            .await
            .map_err(DbError::from)
    }

    async fn create(&self, params: CreateCoreAuthSessionParams<'_>) -> Result<CoreAuthSession, CoreAuthSessionError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(DbError::from)?;
        let user = active_external_user(&mut *tx, params.user_id).await?;
        if user.session_generation != params.session_generation {
            return Err(CoreAuthSessionError::GenerationMismatch);
        }
        let session = sqlx::query_as::<_, CoreAuthSession>(
            "INSERT INTO core_auth_sessions \
             (sid, user_id, current_refresh_hash, previous_refresh_hash, last_rotation_key_hash, last_rotated_at, \
              session_generation, rotation, session_expires_at, revoked_at, revoke_reason, created_at, updated_at) \
             VALUES (?, ?, ?, NULL, NULL, NULL, ?, 0, ?, NULL, NULL, ?, ?) RETURNING *",
        )
        .bind(params.sid)
        .bind(params.user_id)
        .bind(params.current_refresh_hash)
        .bind(params.session_generation)
        .bind(params.session_expires_at)
        .bind(params.now)
        .bind(params.now)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(session)
    }

    async fn rotate(
        &self,
        params: RotateCoreAuthSessionParams<'_>,
    ) -> Result<RotateCoreAuthSessionResult, CoreAuthSessionError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(DbError::from)?;
        let session = session_by_sid(&mut *tx, params.sid)
            .await?
            .ok_or(CoreAuthSessionError::NotFound)?;
        validate_row_state(&session, params.now)?;
        let user = active_external_user(&mut *tx, &session.user_id).await?;
        if user.session_generation != session.session_generation {
            revoke_row(&mut tx, &session.sid, params.now, "generation_mismatch").await?;
            tx.commit().await.map_err(DbError::from)?;
            return Err(CoreAuthSessionError::GenerationMismatch);
        }
        let current_matches = constant_time_eq(
            session.current_refresh_hash.as_bytes(),
            params.presented_secret_hash.as_bytes(),
        );
        let retry_matches = session.previous_refresh_hash.as_deref().is_some_and(|previous| {
            constant_time_eq(previous.as_bytes(), params.presented_secret_hash.as_bytes())
                && session
                    .last_rotation_key_hash
                    .as_deref()
                    .is_some_and(|last_key| constant_time_eq(last_key.as_bytes(), params.rotation_key_hash.as_bytes()))
                && session
                    .last_rotated_at
                    .is_some_and(|last| params.now.saturating_sub(last) <= 60_000)
        });
        if retry_matches {
            tx.commit().await.map_err(DbError::from)?;
            return Ok(RotateCoreAuthSessionResult {
                session,
                username: user.username.unwrap_or_else(|| "external_user".to_owned()),
            });
        }
        if !current_matches {
            revoke_row(&mut tx, &session.sid, params.now, "refresh_replay").await?;
            tx.commit().await.map_err(DbError::from)?;
            return Err(CoreAuthSessionError::Replay);
        }
        let rotated = sqlx::query_as::<_, CoreAuthSession>(
            "UPDATE core_auth_sessions SET previous_refresh_hash = current_refresh_hash, current_refresh_hash = ?, \
             last_rotation_key_hash = ?, last_rotated_at = ?, rotation = rotation + 1, \
             updated_at = ? WHERE sid = ? RETURNING *",
        )
        .bind(params.replacement_secret_hash)
        .bind(params.rotation_key_hash)
        .bind(params.now)
        .bind(params.now)
        .bind(params.sid)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(RotateCoreAuthSessionResult {
            session: rotated,
            username: user.username.unwrap_or_else(|| "external_user".to_owned()),
        })
    }

    async fn validate_access(
        &self,
        sid: &str,
        user_id: &str,
        session_generation: i64,
        rotation: i64,
        now: i64,
    ) -> Result<ActiveCoreAuthSession, CoreAuthSessionError> {
        let mut connection = self.pool.acquire().await.map_err(DbError::from)?;
        let session = session_by_sid(&mut *connection, sid)
            .await?
            .ok_or(CoreAuthSessionError::NotFound)?;
        validate_row_state(&session, now)?;
        if session.user_id != user_id {
            return Err(CoreAuthSessionError::CrossUser);
        }
        if session.session_generation != session_generation || session.rotation != rotation {
            return Err(CoreAuthSessionError::GenerationMismatch);
        }
        let user = active_external_user(&mut *connection, user_id).await?;
        if user.session_generation != session_generation {
            return Err(CoreAuthSessionError::GenerationMismatch);
        }
        Ok(ActiveCoreAuthSession {
            session,
            username: user.username.unwrap_or_else(|| "external_user".to_owned()),
        })
    }

    async fn revoke_matching(
        &self,
        sid: &str,
        presented_secret_hash: &str,
        now: i64,
    ) -> Result<CoreAuthSession, CoreAuthSessionError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(DbError::from)?;
        let session = session_by_sid(&mut *tx, sid)
            .await?
            .ok_or(CoreAuthSessionError::NotFound)?;
        validate_row_state(&session, now)?;
        let current_matches = constant_time_eq(
            session.current_refresh_hash.as_bytes(),
            presented_secret_hash.as_bytes(),
        );
        let recent_previous_matches = session.previous_refresh_hash.as_deref().is_some_and(|previous| {
            constant_time_eq(previous.as_bytes(), presented_secret_hash.as_bytes())
                && session
                    .last_rotated_at
                    .is_some_and(|last| now.saturating_sub(last) <= 60_000)
        });
        if !current_matches && !recent_previous_matches {
            revoke_row(&mut tx, sid, now, "refresh_replay").await?;
            tx.commit().await.map_err(DbError::from)?;
            return Err(CoreAuthSessionError::Replay);
        }
        let revoked = sqlx::query_as::<_, CoreAuthSession>(
            "UPDATE core_auth_sessions SET revoked_at = ?, revoke_reason = 'matching_revoke', updated_at = ? \
             WHERE sid = ? RETURNING *",
        )
        .bind(now)
        .bind(now)
        .bind(sid)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(revoked)
    }

    async fn revoke_user(&self, user_id: &str, now: i64) -> Result<i64, CoreAuthSessionError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(DbError::from)?;
        let generation = sqlx::query_scalar::<_, i64>(
            "UPDATE users SET session_generation = session_generation + 1, updated_at = ? \
             WHERE id = ? AND status = 'active' RETURNING session_generation",
        )
        .bind(now)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?
        .ok_or(CoreAuthSessionError::UserDisabled)?;
        sqlx::query(
            "UPDATE core_auth_sessions SET revoked_at = ?, revoke_reason = 'user_generation_revoke', updated_at = ? \
             WHERE user_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(generation)
    }

    async fn rotate_auth_credentials(&self, params: RotateAuthCredentialsParams<'_>) -> Result<u64, DbError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(DbError::from)?;
        let updated_user = sqlx::query(
            "UPDATE users SET password_hash = ?, jwt_secret = ?, updated_at = ? \
             WHERE id = ? AND user_type = 'local' AND status = 'active' AND password_hash = ?",
        )
        .bind(params.new_password_hash)
        .bind(params.new_jwt_secret)
        .bind(params.now)
        .bind(params.user_id)
        .bind(params.expected_password_hash)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
        if updated_user.rows_affected() != 1 {
            return Err(DbError::Conflict("User credentials changed during rotation".to_owned()));
        }
        let revoked = sqlx::query(
            "UPDATE core_auth_sessions SET revoked_at = ?, revoke_reason = 'jwt_secret_rotation', updated_at = ? \
             WHERE revoked_at IS NULL",
        )
        .bind(params.now)
        .bind(params.now)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?
        .rows_affected();
        tx.commit().await.map_err(DbError::from)?;
        Ok(revoked)
    }

    async fn prune_terminal(&self, now: i64) -> Result<u64, DbError> {
        sqlx::query("DELETE FROM core_auth_sessions WHERE revoked_at IS NOT NULL OR session_expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected())
            .map_err(DbError::from)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for index in 0..max_len {
        diff |= usize::from(left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0));
    }
    diff == 0
}

async fn session_by_sid<'a, A>(executor: A, sid: &str) -> Result<Option<CoreAuthSession>, CoreAuthSessionError>
where
    A: sqlx::Executor<'a, Database = Sqlite>,
{
    sqlx::query_as::<_, CoreAuthSession>("SELECT * FROM core_auth_sessions WHERE sid = ?")
        .bind(sid)
        .fetch_optional(executor)
        .await
        .map_err(DbError::from)
        .map_err(CoreAuthSessionError::from)
}

async fn active_external_user<'a, A>(executor: A, user_id: &str) -> Result<User, CoreAuthSessionError>
where
    A: sqlx::Executor<'a, Database = Sqlite>,
{
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(executor)
        .await
        .map_err(DbError::from)?
        .ok_or(CoreAuthSessionError::NotFound)?;
    if user.user_type != UserType::External {
        return Err(CoreAuthSessionError::CrossUser);
    }
    if user.status == UserStatus::Disabled {
        return Err(CoreAuthSessionError::UserDisabled);
    }
    Ok(user)
}

fn validate_row_state(session: &CoreAuthSession, now: i64) -> Result<(), CoreAuthSessionError> {
    if session.revoked_at.is_some() {
        return Err(CoreAuthSessionError::Revoked);
    }
    if session.session_expires_at <= now {
        return Err(CoreAuthSessionError::Expired);
    }
    Ok(())
}

async fn revoke_row(
    tx: &mut Transaction<'_, Sqlite>,
    sid: &str,
    now: i64,
    reason: &str,
) -> Result<(), CoreAuthSessionError> {
    sqlx::query(
        "UPDATE core_auth_sessions SET revoked_at = ?, revoke_reason = ?, updated_at = ? \
         WHERE sid = ? AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(reason)
    .bind(now)
    .bind(sid)
    .execute(&mut **tx)
    .await
    .map_err(DbError::from)?;
    Ok(())
}
