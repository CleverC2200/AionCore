use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{GeaManagedSkillRow, GeaResourceCatalogRow};
use crate::repository::gea_resource::{IGeaResourceRepository, ReplaceGeaResourceCatalogParams};

#[derive(Clone, Debug)]
pub struct SqliteGeaResourceRepository {
    pool: SqlitePool,
}

impl SqliteGeaResourceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IGeaResourceRepository for SqliteGeaResourceRepository {
    async fn set_active_scope(&self, user_id: &str, tenant_id: &str, environment: &str) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO gea_resource_active_scopes (user_id, tenant_id, environment, updated_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET tenant_id = excluded.tenant_id, \
                environment = excluded.environment, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(environment)
        .bind(aionui_common::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_active_scope(&self, user_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM gea_resource_active_scopes WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn load_catalog(
        &self,
        user_id: &str,
        tenant_id: &str,
        environment: &str,
    ) -> Result<Option<GeaResourceCatalogRow>, DbError> {
        Ok(sqlx::query_as::<_, GeaResourceCatalogRow>(
            "SELECT * FROM gea_resource_catalogs WHERE user_id = ? AND tenant_id = ? AND environment = ?",
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(environment)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn replace_catalog(&self, params: ReplaceGeaResourceCatalogParams<'_>) -> Result<(), DbError> {
        let mut tx = self.pool.begin().await?;
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO gea_resource_catalogs \
                (user_id, tenant_id, environment, revision, server_time, snapshot, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, tenant_id, environment) DO UPDATE SET \
                revision = excluded.revision, server_time = excluded.server_time, \
                snapshot = excluded.snapshot, updated_at = excluded.updated_at",
        )
        .bind(params.user_id)
        .bind(params.tenant_id)
        .bind(params.environment)
        .bind(params.revision)
        .bind(params.server_time)
        .bind(params.snapshot)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM gea_managed_skills WHERE user_id = ? AND tenant_id = ? AND environment = ?")
            .bind(params.user_id)
            .bind(params.tenant_id)
            .bind(params.environment)
            .execute(&mut *tx)
            .await?;

        for skill in params.skills {
            sqlx::query(
                "INSERT INTO gea_managed_skills \
                    (user_id, tenant_id, environment, skill_code, version, name, description, digest, \
                     artifact_size, state, risk_level, path, catalog_revision, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(params.user_id)
            .bind(params.tenant_id)
            .bind(params.environment)
            .bind(skill.skill_code)
            .bind(skill.version)
            .bind(skill.name)
            .bind(skill.description)
            .bind(skill.digest)
            .bind(skill.artifact_size)
            .bind(skill.state)
            .bind(skill.risk_level)
            .bind(skill.path)
            .bind(params.revision)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn list_managed_skills_for_user(&self, user_id: &str) -> Result<Vec<GeaManagedSkillRow>, DbError> {
        Ok(sqlx::query_as::<_, GeaManagedSkillRow>(
            "SELECT skills.* FROM gea_managed_skills skills \
             JOIN gea_resource_active_scopes scope \
               ON scope.user_id = skills.user_id AND scope.tenant_id = skills.tenant_id \
              AND scope.environment = skills.environment \
             WHERE skills.user_id = ? ORDER BY skills.name ASC, skills.skill_code ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn find_managed_skill_for_user(
        &self,
        user_id: &str,
        skill_code: &str,
    ) -> Result<Option<GeaManagedSkillRow>, DbError> {
        Ok(sqlx::query_as::<_, GeaManagedSkillRow>(
            "SELECT skills.* FROM gea_managed_skills skills \
             JOIN gea_resource_active_scopes scope \
               ON scope.user_id = skills.user_id AND scope.tenant_id = skills.tenant_id \
              AND scope.environment = skills.environment \
             WHERE skills.user_id = ? AND skills.skill_code = ? \
             ORDER BY skills.updated_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(skill_code)
        .fetch_optional(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::gea_resource::UpsertGeaManagedSkillParams;

    #[tokio::test]
    async fn replacing_catalog_is_user_scoped_and_removes_withdrawn_skills() {
        let db = crate::init_database_memory().await.unwrap();
        let repo = SqliteGeaResourceRepository::new(db.pool().clone());
        let first = [UpsertGeaManagedSkillParams {
            skill_code: "forecast",
            version: "1.0.0",
            name: "Forecast",
            description: "Forecast data",
            digest: "abcd",
            artifact_size: 12,
            state: "active",
            risk_level: Some("LOW"),
            path: "/tmp/forecast",
        }];
        repo.set_active_scope("system_default_user", "tenant-a", "https://gea.test")
            .await
            .unwrap();
        repo.replace_catalog(ReplaceGeaResourceCatalogParams {
            user_id: "system_default_user",
            tenant_id: "tenant-a",
            environment: "https://gea.test",
            revision: "r1",
            server_time: None,
            snapshot: "{}",
            skills: &first,
        })
        .await
        .unwrap();
        assert_eq!(
            repo.list_managed_skills_for_user("system_default_user")
                .await
                .unwrap()
                .len(),
            1
        );

        repo.replace_catalog(ReplaceGeaResourceCatalogParams {
            user_id: "system_default_user",
            tenant_id: "tenant-a",
            environment: "https://gea.test",
            revision: "r2",
            server_time: None,
            snapshot: "{}",
            skills: &[],
        })
        .await
        .unwrap();
        assert!(
            repo.list_managed_skills_for_user("system_default_user")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
