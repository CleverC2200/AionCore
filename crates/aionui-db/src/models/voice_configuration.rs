use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct VoiceConfigurationRow {
    pub user_id: String,
    pub configuration_encrypted: String,
    pub updated_at: TimestampMs,
}
