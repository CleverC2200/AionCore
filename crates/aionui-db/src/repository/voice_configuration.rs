use crate::error::DbError;
use crate::models::VoiceConfigurationRow;

#[async_trait::async_trait]
pub trait IVoiceConfigurationRepository: Send + Sync {
    async fn get(&self, user_id: &str) -> Result<Option<VoiceConfigurationRow>, DbError>;

    async fn upsert(&self, user_id: &str, configuration_encrypted: &str) -> Result<VoiceConfigurationRow, DbError>;
}
