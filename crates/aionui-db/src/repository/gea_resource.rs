use crate::error::DbError;
use crate::models::{GeaManagedSkillRow, GeaResourceCatalogRow};

#[derive(Debug, Clone)]
pub struct ReplaceGeaResourceCatalogParams<'a> {
    pub user_id: &'a str,
    pub tenant_id: &'a str,
    pub environment: &'a str,
    pub revision: &'a str,
    pub server_time: Option<&'a str>,
    pub snapshot: &'a str,
    pub skills: &'a [UpsertGeaManagedSkillParams<'a>],
}

#[derive(Debug, Clone)]
pub struct UpsertGeaManagedSkillParams<'a> {
    pub skill_code: &'a str,
    pub version: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub digest: &'a str,
    pub artifact_size: i64,
    pub state: &'a str,
    pub risk_level: Option<&'a str>,
    pub path: &'a str,
}

#[async_trait::async_trait]
pub trait IGeaResourceRepository: Send + Sync {
    async fn set_active_scope(&self, user_id: &str, tenant_id: &str, environment: &str) -> Result<(), DbError>;

    async fn clear_active_scope(&self, user_id: &str) -> Result<(), DbError>;

    async fn load_catalog(
        &self,
        user_id: &str,
        tenant_id: &str,
        environment: &str,
    ) -> Result<Option<GeaResourceCatalogRow>, DbError>;

    async fn replace_catalog(&self, params: ReplaceGeaResourceCatalogParams<'_>) -> Result<(), DbError>;

    async fn list_managed_skills_for_user(&self, user_id: &str) -> Result<Vec<GeaManagedSkillRow>, DbError>;

    async fn find_managed_skill_for_user(
        &self,
        user_id: &str,
        skill_code: &str,
    ) -> Result<Option<GeaManagedSkillRow>, DbError>;
}
