use sqlx::SqlitePool;

use crate::DbError;
use crate::models::{ExternalIdentity, ExternalIdentityProvider, User, UserStatus, UserType};
use crate::repository::{
    IExternalIdentityRepository, ProvisionExternalIdentityError, ProvisionExternalIdentityParams,
    ProvisionExternalIdentityResult,
};

#[derive(Clone, Debug)]
pub struct SqliteExternalIdentityRepository {
    pool: SqlitePool,
}

impl SqliteExternalIdentityRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IExternalIdentityRepository for SqliteExternalIdentityRepository {
    async fn provision(
        &self,
        params: ProvisionExternalIdentityParams<'_>,
    ) -> Result<ProvisionExternalIdentityResult, ProvisionExternalIdentityError> {
        // Claim the writer lock before resolving the tuple. Concurrent
        // first-login requests then serialize instead of creating orphan Core
        // users during a deferred read-to-write transaction upgrade.
        let mut connection = self.pool.acquire().await.map_err(DbError::from)?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
            .map_err(DbError::from)?;

        let result: Result<ProvisionExternalIdentityResult, ProvisionExternalIdentityError> = async {
            let existing = find_on_connection(
                &mut connection,
                params.provider,
                params.issuer,
                params.tenant_id,
                params.subject,
            )
            .await?;
            if let Some(identity) = existing {
                let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
                    .bind(&identity.user_id)
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(DbError::from)?
                    .ok_or_else(|| DbError::NotFound("Mapped Core user not found".to_owned()))?;
                if user.user_type != UserType::External {
                    return Err(ProvisionExternalIdentityError::Conflict);
                }
                if user.status == UserStatus::Disabled {
                    return Err(ProvisionExternalIdentityError::CoreUserDisabled);
                }
                return Ok(ProvisionExternalIdentityResult {
                    identity,
                    user,
                    created: false,
                });
            }

            let user_id = aionui_common::generate_prefixed_id("user");
            let identity_id = aionui_common::generate_prefixed_id("external_identity");
            let now = aionui_common::now_ms();
            let user = sqlx::query_as::<_, User>(
                "INSERT INTO users \
                 (id, user_type, external_user_id, username, email, password_hash, avatar_path, \
                  status, session_generation, created_at, updated_at) \
                 VALUES (?, 'external', NULL, NULL, NULL, NULL, NULL, 'active', 0, ?, ?) \
                 RETURNING *",
            )
            .bind(&user_id)
            .bind(now)
            .bind(now)
            .fetch_one(&mut *connection)
            .await
            .map_err(DbError::from)?;
            let identity = sqlx::query_as::<_, ExternalIdentity>(
                "INSERT INTO external_identities \
                 (id, provider, issuer, tenant_id, subject, user_id, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 RETURNING *",
            )
            .bind(&identity_id)
            .bind(params.provider.as_str())
            .bind(params.issuer)
            .bind(params.tenant_id)
            .bind(params.subject)
            .bind(&user_id)
            .bind(now)
            .fetch_one(&mut *connection)
            .await
            .map_err(DbError::from)?;

            Ok(ProvisionExternalIdentityResult {
                identity,
                user,
                created: true,
            })
        }
        .await;

        match result {
            Ok(result) => {
                sqlx::query("COMMIT")
                    .execute(&mut *connection)
                    .await
                    .map_err(DbError::from)?;
                Ok(result)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn find(
        &self,
        provider: ExternalIdentityProvider,
        issuer: &str,
        tenant_id: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, DbError> {
        sqlx::query_as::<_, ExternalIdentity>(
            "SELECT * FROM external_identities \
             WHERE provider = ? AND issuer = ? AND tenant_id = ? AND subject = ?",
        )
        .bind(provider.as_str())
        .bind(issuer)
        .bind(tenant_id)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .map_err(DbError::from)
    }
}

async fn find_on_connection(
    connection: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    provider: ExternalIdentityProvider,
    issuer: &str,
    tenant_id: &str,
    subject: &str,
) -> Result<Option<ExternalIdentity>, DbError> {
    sqlx::query_as::<_, ExternalIdentity>(
        "SELECT * FROM external_identities \
         WHERE provider = ? AND issuer = ? AND tenant_id = ? AND subject = ?",
    )
    .bind(provider.as_str())
    .bind(issuer)
    .bind(tenant_id)
    .bind(subject)
    .fetch_optional(&mut **connection)
    .await
    .map_err(DbError::from)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{ExternalUserProjection, IUserRepository, SqliteUserRepository, init_database, init_database_memory};

    fn params<'a>(tenant_id: &'a str, subject: &'a str) -> ProvisionExternalIdentityParams<'a> {
        ProvisionExternalIdentityParams {
            provider: ExternalIdentityProvider::Lark,
            issuer: "https://open.feishu.cn",
            tenant_id,
            subject,
        }
    }

    #[tokio::test]
    async fn provision_is_atomic_and_idempotent() {
        let db = init_database_memory().await.unwrap();
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());

        let first = identities.provision(params("tenant-1", "subject-1")).await.unwrap();
        let second = identities.provision(params("tenant-1", "subject-1")).await.unwrap();

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.identity.id, second.identity.id);
        assert_eq!(first.user.id, second.user.id);
        assert_eq!(first.user.user_type, UserType::External);
        assert!(first.user.external_user_id.is_none());
        assert!(first.user.password_hash.is_none());
        assert!(first.user.jwt_secret.is_none());
    }

    #[tokio::test]
    async fn provision_rejects_disabled_existing_mapping() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());
        let first = identities
            .provision(params("tenant-disabled", "subject-disabled"))
            .await
            .unwrap();
        users.set_status(&first.user.id, UserStatus::Disabled).await.unwrap();

        let error = identities
            .provision(params("tenant-disabled", "subject-disabled"))
            .await
            .unwrap_err();

        assert!(matches!(error, ProvisionExternalIdentityError::CoreUserDisabled));
    }

    #[tokio::test]
    async fn provision_rejects_tuple_bound_to_an_incompatible_core_user() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());
        let local = users.create_user("legacy-owner", "hash").await.unwrap();
        sqlx::query(
            "INSERT INTO external_identities \
             (id, provider, issuer, tenant_id, subject, user_id, created_at) \
             VALUES ('legacy-mapping', 'lark', 'https://open.feishu.cn', \
                     'tenant-conflict', 'subject-conflict', ?, 1)",
        )
        .bind(&local.id)
        .execute(db.pool())
        .await
        .unwrap();

        let error = identities
            .provision(params("tenant-conflict", "subject-conflict"))
            .await
            .unwrap_err();

        assert!(matches!(error, ProvisionExternalIdentityError::Conflict));
    }

    #[tokio::test]
    async fn concurrent_duplicate_provision_creates_one_user_and_one_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let db = init_database(&dir.path().join("external-identity.db")).await.unwrap();
        let identities = Arc::new(SqliteExternalIdentityRepository::new(db.pool().clone()));
        let first_repo = identities.clone();
        let second_repo = identities.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let first_barrier = barrier.clone();
        let second_barrier = barrier.clone();

        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repo
                .provision(params("tenant-concurrent", "subject-concurrent"))
                .await
                .unwrap()
        });
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repo
                .provision(params("tenant-concurrent", "subject-concurrent"))
                .await
                .unwrap()
        });
        barrier.wait().await;
        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();

        assert_eq!(first.user.id, second.user.id);
        assert_ne!(first.created, second.created);
        let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE user_type = 'external'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let mapping_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM external_identities \
             WHERE tenant_id = 'tenant-concurrent' AND subject = 'subject-concurrent'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(user_count, 1);
        assert_eq!(mapping_count, 1);
    }

    #[tokio::test]
    async fn aionpro_data_adoption_never_rebinds_an_external_identity() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());
        let external = identities
            .provision(params("tenant-adoption", "subject-adoption"))
            .await
            .unwrap();
        let adopter = users
            .ensure_external_user(UserType::Aionpro, "aionpro-1", ExternalUserProjection::default())
            .await
            .unwrap();

        users.adopt_system_default_data(&adopter.id).await.unwrap();

        let resolved = identities
            .find(
                ExternalIdentityProvider::Lark,
                "https://open.feishu.cn",
                "tenant-adoption",
                "subject-adoption",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.user_id, external.user.id);
    }
}
