use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::VoiceConfigurationRow;
use crate::repository::IVoiceConfigurationRepository;

#[derive(Clone, Debug)]
pub struct SqliteVoiceConfigurationRepository {
    pool: SqlitePool,
}

impl SqliteVoiceConfigurationRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IVoiceConfigurationRepository for SqliteVoiceConfigurationRepository {
    async fn get(&self, user_id: &str) -> Result<Option<VoiceConfigurationRow>, DbError> {
        Ok(
            sqlx::query_as::<_, VoiceConfigurationRow>("SELECT * FROM voice_configurations WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn upsert(&self, user_id: &str, configuration_encrypted: &str) -> Result<VoiceConfigurationRow, DbError> {
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO voice_configurations (user_id, configuration_encrypted, updated_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
               configuration_encrypted = excluded.configuration_encrypted, \
               updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(configuration_encrypted)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(VoiceConfigurationRow {
            user_id: user_id.to_owned(),
            configuration_encrypted: configuration_encrypted.to_owned(),
            updated_at: now,
        })
    }
}
