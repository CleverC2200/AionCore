use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct GeaResourceCatalogRow {
    pub user_id: String,
    pub tenant_id: String,
    pub environment: String,
    pub revision: String,
    pub server_time: Option<String>,
    pub snapshot: String,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct GeaManagedSkillRow {
    pub user_id: String,
    pub tenant_id: String,
    pub environment: String,
    pub skill_code: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub digest: String,
    pub artifact_size: i64,
    pub state: String,
    pub risk_level: Option<String>,
    pub path: String,
    pub catalog_revision: String,
    pub updated_at: TimestampMs,
}
