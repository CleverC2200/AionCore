use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ExternalIdentityProvider {
    Lark,
}

impl ExternalIdentityProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lark => "lark",
        }
    }
}

/// Row mapping for the `external_identities` table.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExternalIdentity {
    pub id: String,
    pub provider: ExternalIdentityProvider,
    pub issuer: String,
    pub tenant_id: String,
    pub subject: String,
    pub user_id: String,
    pub created_at: TimestampMs,
}
