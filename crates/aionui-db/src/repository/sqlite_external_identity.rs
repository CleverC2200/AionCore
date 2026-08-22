use sqlx::SqlitePool;

use crate::DbError;
use crate::models::{ExternalIdentity, ExternalIdentityProvider, UserStatus};
use crate::repository::{EnsureExternalIdentityParams, EnsureExternalIdentityResult, IExternalIdentityRepository};

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
    async fn ensure(&self, params: EnsureExternalIdentityParams<'_>) -> Result<EnsureExternalIdentityResult, DbError> {
        let id = aionui_common::generate_prefixed_id("external_identity");
        let now = aionui_common::now_ms();

        let inserted = sqlx::query_as::<_, ExternalIdentity>(
            "INSERT INTO external_identities \
             (id, provider, issuer, tenant_id, subject, user_id, created_at) \
             SELECT ?, ?, ?, ?, ?, ?, ? \
             WHERE EXISTS (SELECT 1 FROM users WHERE id = ? AND status = 'active') \
             ON CONFLICT(provider, issuer, tenant_id, subject) DO NOTHING \
             RETURNING *",
        )
        .bind(&id)
        .bind(params.provider.as_str())
        .bind(params.issuer)
        .bind(params.tenant_id)
        .bind(params.subject)
        .bind(params.user_id)
        .bind(now)
        .bind(params.user_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(identity) = inserted {
            return Ok(EnsureExternalIdentityResult {
                identity,
                created: true,
            });
        }

        if let Some(identity) = self
            .find(params.provider, params.issuer, params.tenant_id, params.subject)
            .await?
        {
            if identity.user_id == params.user_id {
                return Ok(EnsureExternalIdentityResult {
                    identity,
                    created: false,
                });
            }
            return Err(DbError::Conflict(
                "External identity is already mapped to another Core user".to_owned(),
            ));
        }

        let status = sqlx::query_scalar::<_, UserStatus>("SELECT status FROM users WHERE id = ?")
            .bind(params.user_id)
            .fetch_optional(&self.pool)
            .await?;
        match status {
            None => Err(DbError::NotFound("Core user not found".to_owned())),
            Some(UserStatus::Disabled) => Err(DbError::Conflict("Core user is disabled".to_owned())),
            Some(UserStatus::Active) => Err(DbError::Conflict(
                "External identity mapping could not be persisted".to_owned(),
            )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExternalUserProjection, IUserRepository, SqliteUserRepository, UserType, init_database_memory};

    fn params<'a>(user_id: &'a str) -> EnsureExternalIdentityParams<'a> {
        EnsureExternalIdentityParams {
            provider: ExternalIdentityProvider::Lark,
            issuer: "https://open.feishu.cn",
            tenant_id: "tenant-1",
            subject: "subject-1",
            user_id,
        }
    }

    #[tokio::test]
    async fn ensure_is_atomic_and_idempotent_for_the_same_core_user() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());
        let user = users.create_user("lark-user", "hash").await.unwrap();

        let first = identities.ensure(params(&user.id)).await.unwrap();
        let second = identities.ensure(params(&user.id)).await.unwrap();

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.identity.id, second.identity.id);
        assert_eq!(second.identity.user_id, user.id);
    }

    #[tokio::test]
    async fn ensure_rejects_cross_user_remapping() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());
        let first_user = users.create_user("first", "hash").await.unwrap();
        let second_user = users.create_user("second", "hash").await.unwrap();
        identities.ensure(params(&first_user.id)).await.unwrap();

        let error = identities.ensure(params(&second_user.id)).await.unwrap_err();

        assert!(matches!(error, DbError::Conflict(_)));
        let resolved = identities
            .find(
                ExternalIdentityProvider::Lark,
                "https://open.feishu.cn",
                "tenant-1",
                "subject-1",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.user_id, first_user.id);
    }

    #[tokio::test]
    async fn ensure_requires_an_active_existing_core_user() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());

        let missing = identities.ensure(params("missing-user")).await.unwrap_err();
        assert!(matches!(missing, DbError::NotFound(_)));

        let disabled = users.create_user("disabled", "hash").await.unwrap();
        users.set_status(&disabled.id, UserStatus::Disabled).await.unwrap();
        let disabled_error = identities.ensure(params(&disabled.id)).await.unwrap_err();
        assert!(matches!(disabled_error, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn aionpro_data_adoption_never_rebinds_an_external_identity() {
        let db = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(db.pool().clone());
        let identities = SqliteExternalIdentityRepository::new(db.pool().clone());
        identities.ensure(params("system_default_user")).await.unwrap();
        let adopter = users
            .ensure_external_user(UserType::Aionpro, "aionpro-1", ExternalUserProjection::default())
            .await
            .unwrap();

        users.adopt_system_default_data(&adopter.id).await.unwrap();

        let resolved = identities
            .find(
                ExternalIdentityProvider::Lark,
                "https://open.feishu.cn",
                "tenant-1",
                "subject-1",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.user_id, "system_default_user");
    }
}
