use std::sync::Arc;

use aionui_api_types::{SystemSettingsResponse, UpdateSettingsRequest};
use aionui_db::{IClientPreferenceRepository, ISettingsRepository};
use tracing::warn;

use crate::error::SystemError;

/// Supported BCP 47 language codes.
const SUPPORTED_LANGUAGES: &[&str] = &[
    "en-US", "zh-CN", "zh-TW", "ja-JP", "ko-KR", "fr-FR", "de-DE", "es-ES", "pt-BR", "ru-RU", "ar-SA", "it-IT",
    "nl-NL", "pl-PL", "tr-TR", "vi-VN", "th-TH", "id-ID",
];

/// Client-preference key that is the single source of truth for the UI
/// language. `system_settings.language` is a legacy read fallback only;
/// writes always land here (settings-dedup B1).
const LANGUAGE_PREF_KEY: &str = "language";

/// Business logic for system settings (language, notifications, etc.).
///
/// Language is proxied to `client_preferences['language']` — the same key the
/// frontend reads/writes via `/api/settings/client` — so there is exactly one
/// stored truth. The remaining boolean switches still live in
/// `system_settings` until the B2 column migration.
#[derive(Clone)]
pub struct SettingsService {
    repo: Arc<dyn ISettingsRepository>,
    pref_repo: Arc<dyn IClientPreferenceRepository>,
}

impl SettingsService {
    pub fn new(repo: Arc<dyn ISettingsRepository>, pref_repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self { repo, pref_repo }
    }

    /// Get current system settings, falling back to defaults if not yet persisted.
    pub async fn get_settings(&self, user_id: &str) -> Result<SystemSettingsResponse, SystemError> {
        let row = self
            .repo
            .get_settings(user_id)
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to get settings: {e}")))?;

        let mut settings = row.map_or_else(SystemSettingsResponse::default, |s| SystemSettingsResponse {
            language: s.language,
            notification_enabled: s.notification_enabled,
            cron_notification_enabled: s.cron_notification_enabled,
            command_queue_enabled: s.command_queue_enabled,
            save_upload_to_workspace: s.save_upload_to_workspace,
        });
        if let Some(language) = self.get_language_preference(user_id).await? {
            settings.language = language;
        }
        Ok(settings)
    }

    /// Partially update system settings. Only fields present in the request are changed.
    pub async fn update_settings(
        &self,
        user_id: &str,
        req: UpdateSettingsRequest,
    ) -> Result<SystemSettingsResponse, SystemError> {
        if let Some(ref lang) = req.language {
            validate_language(lang)?;
        }

        // Merge with current settings (or defaults)
        let current = self.get_settings(user_id).await?;

        let language = req.language.unwrap_or(current.language);
        let notification_enabled = req.notification_enabled.unwrap_or(current.notification_enabled);
        let cron_notification_enabled = req
            .cron_notification_enabled
            .unwrap_or(current.cron_notification_enabled);
        let command_queue_enabled = req.command_queue_enabled.unwrap_or(current.command_queue_enabled);
        let save_upload_to_workspace = req.save_upload_to_workspace.unwrap_or(current.save_upload_to_workspace);

        // Language truth lives in client_preferences; the column write below
        // only keeps the legacy fallback convergent for pre-B1 readers.
        let serialized = serde_json::Value::String(language.clone()).to_string();
        self.pref_repo
            .upsert_batch(user_id, &[(LANGUAGE_PREF_KEY, serialized.as_str())])
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to update language preference: {e}")))?;

        let row = self
            .repo
            .upsert_settings(
                user_id,
                &language,
                notification_enabled,
                cron_notification_enabled,
                command_queue_enabled,
                save_upload_to_workspace,
            )
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to update settings: {e}")))?;

        Ok(SystemSettingsResponse {
            language,
            notification_enabled: row.notification_enabled,
            cron_notification_enabled: row.cron_notification_enabled,
            command_queue_enabled: row.command_queue_enabled,
            save_upload_to_workspace: row.save_upload_to_workspace,
        })
    }

    /// Read the language preference, tolerating both JSON-encoded and raw
    /// string storage. Non-string or empty values are ignored (legacy column
    /// then serves as the fallback).
    async fn get_language_preference(&self, user_id: &str) -> Result<Option<String>, SystemError> {
        let rows = self
            .pref_repo
            .get_by_keys(user_id, &[LANGUAGE_PREF_KEY])
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to get language preference: {e}")))?;
        let Some(row) = rows.into_iter().find(|row| row.key == LANGUAGE_PREF_KEY) else {
            return Ok(None);
        };
        let value = match serde_json::from_str::<serde_json::Value>(&row.value) {
            Ok(serde_json::Value::String(s)) => s,
            Ok(_) => {
                warn!(
                    key = LANGUAGE_PREF_KEY,
                    "Ignoring non-string stored language preference"
                );
                return Ok(None);
            }
            // Raw (non-JSON) storage from older writers.
            Err(_) => row.value,
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(trimmed.to_owned()))
    }
}

fn validate_language(lang: &str) -> Result<(), SystemError> {
    if SUPPORTED_LANGUAGES.contains(&lang) {
        Ok(())
    } else {
        Err(SystemError::BadRequest(format!("Unsupported language code: '{lang}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "user-1";
    use aionui_db::{SqliteClientPreferenceRepository, SqliteSettingsRepository, init_database_memory};

    async fn setup() -> SettingsService {
        setup_with_prefs().await.0
    }

    async fn setup_with_prefs() -> (SettingsService, Arc<SqliteClientPreferenceRepository>) {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, '', 'active', 0, 1, 1)",
        )
        .bind(TEST_USER_ID)
        .bind(TEST_USER_ID)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = Arc::new(SqliteSettingsRepository::new(db.pool().clone()));
        let pref_repo = Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        // Leak the db handle so the pool stays alive for the test
        std::mem::forget(db);
        (SettingsService::new(repo, pref_repo.clone()), pref_repo)
    }

    #[test]
    fn validate_language_accepts_supported() {
        assert!(validate_language("en-US").is_ok());
        assert!(validate_language("zh-CN").is_ok());
        assert!(validate_language("ja-JP").is_ok());
    }

    #[test]
    fn validate_language_rejects_unsupported() {
        assert!(validate_language("invalid").is_err());
        assert!(validate_language("").is_err());
        assert!(validate_language("xx-YY").is_err());
    }

    #[tokio::test]
    async fn get_settings_returns_defaults_when_empty() {
        let svc = setup().await;
        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings, SystemSettingsResponse::default());
    }

    #[tokio::test]
    async fn update_single_field() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            language: Some("zh-CN".into()),
            ..Default::default()
        };
        let result = svc.update_settings(TEST_USER_ID, req).await.unwrap();
        assert_eq!(result.language, "zh-CN");
        // Other fields stay at defaults
        assert!(result.notification_enabled);
        assert!(!result.cron_notification_enabled);
    }

    #[tokio::test]
    async fn update_multiple_fields() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            notification_enabled: Some(false),
            command_queue_enabled: Some(true),
            ..Default::default()
        };
        let result = svc.update_settings(TEST_USER_ID, req).await.unwrap();
        assert!(!result.notification_enabled);
        assert!(result.command_queue_enabled);
        assert_eq!(result.language, "en-US");
    }

    #[tokio::test]
    async fn update_empty_request_returns_current() {
        let svc = setup().await;
        let result = svc
            .update_settings(TEST_USER_ID, UpdateSettingsRequest::default())
            .await
            .unwrap();
        assert_eq!(result, SystemSettingsResponse::default());
    }

    #[tokio::test]
    async fn update_invalid_language_rejected() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            language: Some("invalid-lang".into()),
            ..Default::default()
        };
        let err = svc.update_settings(TEST_USER_ID, req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn language_preference_wins_over_legacy_column() {
        let (svc, _prefs) = setup_with_prefs().await;
        // Legacy column says zh-CN…
        svc.repo
            .upsert_settings(TEST_USER_ID, "zh-CN", true, false, false, false)
            .await
            .unwrap();
        // …but the preference (single truth) says ja-JP.
        svc.pref_repo
            .upsert_batch(TEST_USER_ID, &[(LANGUAGE_PREF_KEY, "\"ja-JP\"")])
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings.language, "ja-JP");
    }

    #[tokio::test]
    async fn language_falls_back_to_legacy_column_without_preference() {
        let (svc, _prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "zh-TW", true, false, false, false)
            .await
            .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings.language, "zh-TW");
    }

    #[tokio::test]
    async fn update_language_writes_the_preference_truth() {
        let (svc, prefs) = setup_with_prefs().await;
        let req = UpdateSettingsRequest {
            language: Some("ko-KR".into()),
            ..Default::default()
        };
        let result = svc.update_settings(TEST_USER_ID, req).await.unwrap();
        assert_eq!(result.language, "ko-KR");

        // The preference row is the stored truth (JSON-encoded string).
        let rows = prefs.get_by_keys(TEST_USER_ID, &[LANGUAGE_PREF_KEY]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "\"ko-KR\"");
        // And reads agree with it.
        assert_eq!(svc.get_settings(TEST_USER_ID).await.unwrap().language, "ko-KR");
    }

    #[tokio::test]
    async fn raw_string_preference_storage_is_tolerated() {
        let (svc, prefs) = setup_with_prefs().await;
        // Older writers stored the raw string without JSON encoding.
        prefs
            .upsert_batch(TEST_USER_ID, &[(LANGUAGE_PREF_KEY, "fr-FR")])
            .await
            .unwrap();

        assert_eq!(svc.get_settings(TEST_USER_ID).await.unwrap().language, "fr-FR");
    }

    #[tokio::test]
    async fn non_string_language_preference_is_ignored() {
        let (svc, prefs) = setup_with_prefs().await;
        svc.repo
            .upsert_settings(TEST_USER_ID, "zh-CN", true, false, false, false)
            .await
            .unwrap();
        prefs
            .upsert_batch(TEST_USER_ID, &[(LANGUAGE_PREF_KEY, "123")])
            .await
            .unwrap();

        // Falls back to the legacy column.
        assert_eq!(svc.get_settings(TEST_USER_ID).await.unwrap().language, "zh-CN");
    }

    #[tokio::test]
    async fn update_then_get_reflects_changes() {
        let svc = setup().await;
        svc.update_settings(
            TEST_USER_ID,
            UpdateSettingsRequest {
                language: Some("ja-JP".into()),
                save_upload_to_workspace: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let settings = svc.get_settings(TEST_USER_ID).await.unwrap();
        assert_eq!(settings.language, "ja-JP");
        assert!(settings.save_upload_to_workspace);
    }
}
