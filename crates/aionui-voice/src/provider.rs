use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use aionui_api_types::{
    ManagedVoiceCapability, ManagedVoiceConfigurationResponse, ManagedVoiceConfigurationSource,
    ManagedVoiceHealthResponse, ManagedVoiceHealthStatus, ManagedVoiceProvider, ManagedVoiceSessionMode,
    UpdateManagedVoiceConfigurationRequest, VoiceSessionRtcCredentials,
};
use aionui_common::{decrypt_string, encrypt_string, now_ms};
use aionui_db::IVoiceConfigurationRepository;
use tokio::sync::Mutex;

const RTC_API_HOST: &str = "rtc.volcengineapi.com";
const RTC_API_ENDPOINT: &str = "https://rtc.volcengineapi.com";
const RTC_API_VERSION: &str = "2024-12-01";
const RTC_REGION: &str = "cn-north-1";
const RTC_SERVICE: &str = "rtc";

const ACCESS_KEY_ENV: &str = "VOLC_ACCESSKEY";
const SECRET_KEY_ENV: &str = "VOLC_SECRETKEY";
const RTC_APP_ID_ENV: &str = "AIONUI_VOLCENGINE_RTC_APP_ID";
const RTC_APP_KEY_ENV: &str = "AIONUI_VOLCENGINE_RTC_APP_KEY";
const VOICE_CHAT_CONFIG_ENV: &str = "AIONUI_VOLCENGINE_VOICE_CHAT_CONFIG";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct VoiceProviderSession {
    pub session_id: String,
    pub room_id: String,
    pub user_id: String,
    pub task_id: String,
    pub expires_at: i64,
    pub mode: ManagedVoiceSessionMode,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("voice configuration was not found")]
    NotFound,
    #[error("voice configuration is managed and read-only")]
    Managed,
    #[error("provider configuration is invalid")]
    InvalidConfiguration,
    #[error("provider configuration storage is unavailable")]
    Storage,
    #[error("provider request could not be sent")]
    Transport,
    #[error("provider rejected the request")]
    Rejected,
    #[error("provider response is invalid")]
    InvalidResponse,
}

impl ProviderError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Managed => "managed",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::Storage => "storage",
            Self::Transport => "transport",
            Self::Rejected => "rejected",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

#[async_trait]
pub trait ManagedVoiceBackend: Send + Sync {
    fn capability(&self) -> ManagedVoiceCapability;

    async fn prepare_session(
        &self,
        session: &VoiceProviderSession,
    ) -> Result<VoiceSessionRtcCredentials, ProviderError>;

    async fn start_session(&self, session: &VoiceProviderSession) -> Result<(), ProviderError>;

    async fn stop_session(&self, session: &VoiceProviderSession) -> Result<(), ProviderError>;

    async fn interrupt_session(&self, session: &VoiceProviderSession) -> Result<(), ProviderError>;

    async fn speak_text(&self, session: &VoiceProviderSession, text: &str) -> Result<(), ProviderError>;
}

pub fn provider_from_environment() -> Arc<dyn ManagedVoiceBackend> {
    match VolcengineVoiceBackend::from_environment() {
        Ok(provider) => Arc::new(provider),
        Err(error) => Arc::new(DisabledVoiceBackend {
            reason: error.reason().to_owned(),
        }),
    }
}

struct DisabledVoiceBackend {
    reason: String,
}

#[async_trait]
impl ManagedVoiceBackend for DisabledVoiceBackend {
    fn capability(&self) -> ManagedVoiceCapability {
        ManagedVoiceCapability {
            enabled: false,
            provider: Some(ManagedVoiceProvider::VolcengineRtc),
            reason: Some(self.reason.clone()),
        }
    }

    async fn prepare_session(
        &self,
        _session: &VoiceProviderSession,
    ) -> Result<VoiceSessionRtcCredentials, ProviderError> {
        Err(ProviderError::InvalidConfiguration)
    }

    async fn start_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
        Err(ProviderError::InvalidConfiguration)
    }

    async fn stop_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
        Err(ProviderError::InvalidConfiguration)
    }

    async fn interrupt_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
        Err(ProviderError::InvalidConfiguration)
    }

    async fn speak_text(&self, _session: &VoiceProviderSession, _text: &str) -> Result<(), ProviderError> {
        Err(ProviderError::InvalidConfiguration)
    }
}

#[derive(Debug)]
enum ProviderConfigError {
    Missing,
    Invalid,
}

impl ProviderConfigError {
    fn reason(&self) -> &'static str {
        match self {
            Self::Missing => "not_configured",
            Self::Invalid => "invalid_configuration",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VolcengineVoiceConfig {
    access_key: String,
    secret_key: String,
    rtc_app_id: String,
    rtc_app_key: String,
    voice_chat_template: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedVoiceConfiguration {
    id: String,
    name: String,
    enabled: bool,
    config: VolcengineVoiceConfig,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredVoiceCatalog {
    version: u8,
    configurations: Vec<SavedVoiceConfiguration>,
}

impl Default for StoredVoiceCatalog {
    fn default() -> Self {
        Self {
            version: 1,
            configurations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacySavedVoiceConfiguration {
    enabled: bool,
    config: VolcengineVoiceConfig,
}

impl VolcengineVoiceConfig {
    fn from_environment() -> Result<Self, ProviderConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, ProviderConfigError> {
        let required = |value: Option<String>| {
            value
                .filter(|value| !value.trim().is_empty())
                .ok_or(ProviderConfigError::Missing)
        };
        let access_key = required(lookup(ACCESS_KEY_ENV))?;
        let secret_key = required(lookup(SECRET_KEY_ENV))?;
        let rtc_app_id = required(lookup(RTC_APP_ID_ENV))?;
        let rtc_app_key = required(lookup(RTC_APP_KEY_ENV))?;
        let voice_chat_raw = required(lookup(VOICE_CHAT_CONFIG_ENV))?;

        if rtc_app_id.len() != 24 {
            return Err(ProviderConfigError::Invalid);
        }
        let voice_chat_template: Value =
            serde_json::from_str(&voice_chat_raw).map_err(|_| ProviderConfigError::Invalid)?;
        let object = voice_chat_template.as_object().ok_or(ProviderConfigError::Invalid)?;
        let agent_user_id = object
            .get("AgentConfig")
            .and_then(Value::as_object)
            .and_then(|agent| agent.get("UserId"))
            .and_then(Value::as_str)
            .filter(|user_id| !user_id.trim().is_empty());
        if agent_user_id.is_none() || !object.get("Config").is_some_and(Value::is_object) {
            return Err(ProviderConfigError::Invalid);
        }

        Ok(Self {
            access_key,
            secret_key,
            rtc_app_id,
            rtc_app_key,
            voice_chat_template,
        })
    }

    fn start_body(&self, session: &VoiceProviderSession) -> Result<Value, ProviderError> {
        let mut body = self.voice_chat_template.clone();
        let object = body.as_object_mut().ok_or(ProviderError::InvalidConfiguration)?;
        object.insert("AppId".to_owned(), json!(self.rtc_app_id));
        object.insert("RoomId".to_owned(), json!(session.room_id));
        object.insert("TaskId".to_owned(), json!(session.task_id));
        let agent = object
            .get_mut("AgentConfig")
            .and_then(Value::as_object_mut)
            .ok_or(ProviderError::InvalidConfiguration)?;
        agent.insert("TargetUserId".to_owned(), json!([session.user_id]));
        if session.mode == ManagedVoiceSessionMode::Dictation {
            agent.remove("WelcomeMessage");
        }
        Ok(body)
    }

    fn from_update(
        request: UpdateManagedVoiceConfigurationRequest,
        current: Option<&Self>,
    ) -> Result<Self, ProviderError> {
        let required = |value: String| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(ProviderError::InvalidConfiguration)
            } else {
                Ok(trimmed.to_owned())
            }
        };
        let secret = |value: Option<String>, existing: Option<&str>| {
            value
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_owned())
                .or_else(|| existing.map(str::to_owned))
                .ok_or(ProviderError::InvalidConfiguration)
        };

        let rtc_app_id = required(request.rtc_app_id)?;
        if rtc_app_id.len() != 24 {
            return Err(ProviderError::InvalidConfiguration);
        }
        let access_key = secret(request.access_key, current.map(|value| value.access_key.as_str()))?;
        let secret_key = secret(request.secret_key, current.map(|value| value.secret_key.as_str()))?;
        let rtc_app_key = secret(request.rtc_app_key, current.map(|value| value.rtc_app_key.as_str()))?;
        let llm_api_key = secret(
            request.llm_api_key,
            current.and_then(|value| value.string_at(&["Config", "LLMConfig", "APIKey"])),
        )?;

        let agent_user_id = required(request.agent_user_id)?;
        let asr_app_id = required(request.asr_app_id)?;
        let asr_cluster = required(request.asr_cluster)?;
        let tts_app_id = required(request.tts_app_id)?;
        let tts_cluster = required(request.tts_cluster)?;
        let tts_voice_type = required(request.tts_voice_type)?;
        let llm_url = required(request.llm_url)?;
        let llm_model_name = required(request.llm_model_name)?;

        let voice_chat_template = json!({
            "AgentConfig": {
                "TargetUserId": [],
                "WelcomeMessage": request.welcome_message,
                "UserId": agent_user_id,
                "EnableConversationStateCallback": true
            },
            "Config": {
                "ASRConfig": {
                    "Provider": "volcano",
                    "ProviderParams": {
                        "Mode": "smallmodel",
                        "AppId": asr_app_id,
                        "Cluster": asr_cluster
                    }
                },
                "TTSConfig": {
                    "Provider": "volcano",
                    "ProviderParams": {
                        "app": { "appid": tts_app_id, "cluster": tts_cluster },
                        "audio": {
                            "voice_type": tts_voice_type,
                            "speed_ratio": 1,
                            "pitch_ratio": 1,
                            "volume_ratio": 1
                        }
                    }
                },
                "LLMConfig": {
                    "Mode": "CustomLLM",
                    "URL": llm_url,
                    "APIKey": llm_api_key,
                    "ModelName": llm_model_name,
                    "SystemMessages": [request.system_message],
                    "VisionConfig": { "Enable": false }
                },
                "InterruptMode": 0
            }
        });

        Ok(Self {
            access_key,
            secret_key,
            rtc_app_id,
            rtc_app_key,
            voice_chat_template,
        })
    }

    fn string_at(&self, path: &[&str]) -> Option<&str> {
        let mut value = &self.voice_chat_template;
        for key in path {
            value = value.get(*key)?;
        }
        value.as_str()
    }

    fn response(
        &self,
        id: String,
        name: String,
        enabled: bool,
        source: ManagedVoiceConfigurationSource,
        created_at: i64,
        updated_at: i64,
    ) -> ManagedVoiceConfigurationResponse {
        let text = |path: &[&str]| self.string_at(path).unwrap_or_default().to_owned();
        let system_message = self
            .voice_chat_template
            .pointer("/Config/LLMConfig/SystemMessages/0")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        ManagedVoiceConfigurationResponse {
            id,
            name,
            enabled,
            provider: ManagedVoiceProvider::VolcengineRtc,
            source,
            rtc_app_id: self.rtc_app_id.clone(),
            access_key_configured: !self.access_key.is_empty(),
            secret_key_configured: !self.secret_key.is_empty(),
            rtc_app_key_configured: !self.rtc_app_key.is_empty(),
            agent_user_id: text(&["AgentConfig", "UserId"]),
            welcome_message: text(&["AgentConfig", "WelcomeMessage"]),
            asr_app_id: text(&["Config", "ASRConfig", "ProviderParams", "AppId"]),
            asr_cluster: text(&["Config", "ASRConfig", "ProviderParams", "Cluster"]),
            tts_app_id: text(&["Config", "TTSConfig", "ProviderParams", "app", "appid"]),
            tts_cluster: text(&["Config", "TTSConfig", "ProviderParams", "app", "cluster"]),
            tts_voice_type: text(&["Config", "TTSConfig", "ProviderParams", "audio", "voice_type"]),
            llm_url: text(&["Config", "LLMConfig", "URL"]),
            llm_api_key_configured: self
                .string_at(&["Config", "LLMConfig", "APIKey"])
                .is_some_and(|v| !v.is_empty()),
            llm_model_name: text(&["Config", "LLMConfig", "ModelName"]),
            system_message,
            created_at,
            updated_at,
        }
    }
}

pub struct VoiceProviderRegistry {
    repo: Arc<dyn IVoiceConfigurationRepository>,
    encryption_key: [u8; 32],
    environment: Option<VolcengineVoiceConfig>,
    mutation_lock: Mutex<()>,
}

impl VoiceProviderRegistry {
    pub fn new(repo: Arc<dyn IVoiceConfigurationRepository>, encryption_key: [u8; 32]) -> Self {
        Self::with_environment(repo, encryption_key, VolcengineVoiceConfig::from_environment().ok())
    }

    fn with_environment(
        repo: Arc<dyn IVoiceConfigurationRepository>,
        encryption_key: [u8; 32],
        environment: Option<VolcengineVoiceConfig>,
    ) -> Self {
        Self {
            repo,
            encryption_key,
            environment,
            mutation_lock: Mutex::new(()),
        }
    }

    async fn saved_catalog(&self, user_id: &str) -> Result<Option<StoredVoiceCatalog>, ProviderError> {
        let Some(row) = self.repo.get(user_id).await.map_err(|error| {
            tracing::warn!(error = %error, "managed voice configuration read failed");
            ProviderError::Storage
        })?
        else {
            return Ok(None);
        };
        let plaintext = match decrypt_string(&row.configuration_encrypted, &self.encryption_key) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(error = %error, "managed voice configuration decryption failed");
                return Err(ProviderError::Storage);
            }
        };
        if let Ok(catalog) = serde_json::from_str::<StoredVoiceCatalog>(&plaintext) {
            return Ok(Some(catalog));
        }
        match serde_json::from_str::<LegacySavedVoiceConfiguration>(&plaintext) {
            Ok(legacy) => Ok(Some(StoredVoiceCatalog {
                version: 1,
                configurations: vec![SavedVoiceConfiguration {
                    id: "voice-default".to_owned(),
                    name: "Volcengine Realtime Voice".to_owned(),
                    enabled: legacy.enabled,
                    config: legacy.config,
                    created_at: row.updated_at,
                    updated_at: row.updated_at,
                }],
            })),
            Err(error) => {
                tracing::warn!(error = %error, "managed voice configuration payload is invalid");
                Err(ProviderError::Storage)
            }
        }
    }

    async fn persist_catalog(&self, user_id: &str, catalog: &StoredVoiceCatalog) -> Result<(), ProviderError> {
        let plaintext = serde_json::to_string(catalog).map_err(|_| ProviderError::InvalidConfiguration)?;
        let encrypted =
            encrypt_string(&plaintext, &self.encryption_key).map_err(|_| ProviderError::InvalidConfiguration)?;
        self.repo
            .upsert(user_id, &encrypted)
            .await
            .map(|_| ())
            .map_err(|_| ProviderError::InvalidConfiguration)
    }

    fn environment_configuration(&self) -> Option<SavedVoiceConfiguration> {
        self.environment.clone().map(|config| SavedVoiceConfiguration {
            id: "environment".to_owned(),
            name: "Volcengine Realtime Voice".to_owned(),
            enabled: true,
            config,
            created_at: 0,
            updated_at: 0,
        })
    }

    async fn resolved(
        &self,
        user_id: &str,
    ) -> Result<Option<(SavedVoiceConfiguration, ManagedVoiceConfigurationSource)>, ProviderError> {
        if let Some(catalog) = self.saved_catalog(user_id).await?
            && let Some(configuration) = catalog
                .configurations
                .into_iter()
                .find(|configuration| configuration.enabled)
        {
            return Ok(Some((configuration, ManagedVoiceConfigurationSource::Saved)));
        }
        Ok(self
            .environment_configuration()
            .map(|configuration| (configuration, ManagedVoiceConfigurationSource::Environment)))
    }

    pub async fn capability(&self, user_id: &str) -> ManagedVoiceCapability {
        match self.resolved(user_id).await {
            Ok(Some((saved, _))) if saved.enabled => ManagedVoiceCapability {
                enabled: true,
                provider: Some(ManagedVoiceProvider::VolcengineRtc),
                reason: None,
            },
            Ok(Some(_)) => ManagedVoiceCapability {
                enabled: false,
                provider: Some(ManagedVoiceProvider::VolcengineRtc),
                reason: Some("disabled".to_owned()),
            },
            Ok(None) => ManagedVoiceCapability {
                enabled: false,
                provider: Some(ManagedVoiceProvider::VolcengineRtc),
                reason: Some(
                    if self
                        .saved_catalog(user_id)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|catalog| !catalog.configurations.is_empty())
                    {
                        "disabled"
                    } else {
                        "not_configured"
                    }
                    .to_owned(),
                ),
            },
            Err(_) => ManagedVoiceCapability {
                enabled: false,
                provider: Some(ManagedVoiceProvider::VolcengineRtc),
                reason: Some("configuration_unavailable".to_owned()),
            },
        }
    }

    pub async fn configurations(&self, user_id: &str) -> Result<Vec<ManagedVoiceConfigurationResponse>, ProviderError> {
        if let Some(catalog) = self.saved_catalog(user_id).await? {
            let has_active_saved = catalog.configurations.iter().any(|configuration| configuration.enabled);
            let mut configurations: Vec<_> = catalog
                .configurations
                .into_iter()
                .map(|saved| {
                    saved.config.response(
                        saved.id,
                        saved.name,
                        saved.enabled,
                        ManagedVoiceConfigurationSource::Saved,
                        saved.created_at,
                        saved.updated_at,
                    )
                })
                .collect();
            if let Some(mut environment) = self.environment_configuration() {
                environment.enabled = !has_active_saved;
                configurations.insert(
                    0,
                    environment.config.response(
                        environment.id,
                        environment.name,
                        environment.enabled,
                        ManagedVoiceConfigurationSource::Environment,
                        environment.created_at,
                        environment.updated_at,
                    ),
                );
            }
            return Ok(configurations);
        }
        Ok(self
            .environment_configuration()
            .map(|saved| {
                saved.config.response(
                    saved.id,
                    saved.name,
                    saved.enabled,
                    ManagedVoiceConfigurationSource::Environment,
                    saved.created_at,
                    saved.updated_at,
                )
            })
            .into_iter()
            .collect())
    }

    pub async fn create(
        &self,
        user_id: &str,
        request: UpdateManagedVoiceConfigurationRequest,
    ) -> Result<ManagedVoiceConfigurationResponse, ProviderError> {
        let _guard = self.mutation_lock.lock().await;
        let mut catalog = self.saved_catalog(user_id).await?.unwrap_or_default();
        let name = required_name(&request.name)?;
        let enabled = request.enabled;
        let config = VolcengineVoiceConfig::from_update(request, None)?;
        if enabled {
            catalog.configurations.iter_mut().for_each(|item| item.enabled = false);
        }
        let timestamp = now_ms();
        let saved = SavedVoiceConfiguration {
            id: format!("voice-{}", uuid::Uuid::now_v7().simple()),
            name,
            enabled,
            config,
            created_at: timestamp,
            updated_at: timestamp,
        };
        catalog.configurations.push(saved.clone());
        self.persist_catalog(user_id, &catalog).await?;
        Ok(saved_response(saved))
    }

    pub async fn update(
        &self,
        user_id: &str,
        configuration_id: &str,
        request: UpdateManagedVoiceConfigurationRequest,
    ) -> Result<ManagedVoiceConfigurationResponse, ProviderError> {
        if configuration_id == "environment" {
            return Err(ProviderError::Managed);
        }
        let _guard = self.mutation_lock.lock().await;
        let mut catalog = self.saved_catalog(user_id).await?.unwrap_or_default();
        let position = catalog
            .configurations
            .iter()
            .position(|item| item.id == configuration_id)
            .ok_or(ProviderError::NotFound)?;
        let current = catalog
            .configurations
            .get(position)
            .map(|item| &item.config)
            .ok_or(ProviderError::NotFound)?;
        let name = required_name(&request.name)?;
        let enabled = request.enabled;
        let config = VolcengineVoiceConfig::from_update(request, Some(current))?;
        if enabled {
            catalog.configurations.iter_mut().for_each(|item| item.enabled = false);
        }
        let timestamp = now_ms();
        let item = &mut catalog.configurations[position];
        item.name = name;
        item.enabled = enabled;
        item.config = config;
        item.updated_at = timestamp;
        let saved = item.clone();
        self.persist_catalog(user_id, &catalog).await?;
        Ok(saved_response(saved))
    }

    pub async fn set_enabled(
        &self,
        user_id: &str,
        configuration_id: &str,
        enabled: bool,
    ) -> Result<ManagedVoiceConfigurationResponse, ProviderError> {
        if configuration_id == "environment" {
            return Err(ProviderError::Managed);
        }
        let _guard = self.mutation_lock.lock().await;
        let mut catalog = self.saved_catalog(user_id).await?.unwrap_or_default();
        let timestamp = now_ms();
        let position = catalog
            .configurations
            .iter()
            .position(|item| item.id == configuration_id);
        if enabled {
            catalog.configurations.iter_mut().for_each(|item| item.enabled = false);
        }
        let saved = if let Some(position) = position {
            let item = &mut catalog.configurations[position];
            item.enabled = enabled;
            item.updated_at = timestamp;
            item.clone()
        } else {
            return Err(ProviderError::NotFound);
        };
        self.persist_catalog(user_id, &catalog).await?;
        Ok(saved_response(saved))
    }

    pub async fn delete(&self, user_id: &str, configuration_id: &str) -> Result<(), ProviderError> {
        if configuration_id == "environment" {
            return Err(ProviderError::Managed);
        }
        let _guard = self.mutation_lock.lock().await;
        let mut catalog = self.saved_catalog(user_id).await?.ok_or(ProviderError::NotFound)?;
        let length = catalog.configurations.len();
        catalog.configurations.retain(|item| item.id != configuration_id);
        if catalog.configurations.len() == length {
            return Err(ProviderError::NotFound);
        }
        self.persist_catalog(user_id, &catalog).await
    }

    pub async fn health(
        &self,
        user_id: &str,
        configuration_id: &str,
    ) -> Result<ManagedVoiceHealthResponse, ProviderError> {
        let config = if configuration_id == "environment" {
            self.environment.clone()
        } else if let Some(catalog) = self.saved_catalog(user_id).await? {
            catalog
                .configurations
                .into_iter()
                .find(|item| item.id == configuration_id)
                .map(|item| item.config)
        } else {
            None
        }
        .ok_or(ProviderError::NotFound)?;
        let backend = VolcengineVoiceBackend::from_config(config);
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let session = VoiceProviderSession {
            session_id: format!("voice-health-{suffix}"),
            room_id: format!("aion-health-{suffix}"),
            user_id: format!("health-{suffix}"),
            task_id: format!("voice-health-{suffix}"),
            expires_at: now_ms() + 5 * 60 * 1000,
            mode: ManagedVoiceSessionMode::Dictation,
        };
        let started_at = Instant::now();
        let result = match backend.start_session(&session).await {
            Ok(()) => backend.stop_session(&session).await,
            Err(error) => Err(error),
        };
        let latency_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(match result {
            Ok(()) => ManagedVoiceHealthResponse {
                status: ManagedVoiceHealthStatus::Healthy,
                latency_ms,
                error_code: None,
            },
            Err(error) => ManagedVoiceHealthResponse {
                status: ManagedVoiceHealthStatus::Unhealthy,
                latency_ms,
                error_code: Some(error.kind().to_owned()),
            },
        })
    }

    pub async fn backend(&self, user_id: &str) -> Arc<dyn ManagedVoiceBackend> {
        match self.resolved(user_id).await {
            Ok(Some((saved, _))) if saved.enabled => Arc::new(VolcengineVoiceBackend::from_config(saved.config)),
            Ok(Some(_)) => Arc::new(DisabledVoiceBackend {
                reason: "disabled".to_owned(),
            }),
            Ok(None) => Arc::new(DisabledVoiceBackend {
                reason: "not_configured".to_owned(),
            }),
            Err(_) => Arc::new(DisabledVoiceBackend {
                reason: "configuration_unavailable".to_owned(),
            }),
        }
    }
}

fn required_name(value: &str) -> Result<String, ProviderError> {
    let value = value.trim();
    if value.is_empty() {
        Err(ProviderError::InvalidConfiguration)
    } else {
        Ok(value.to_owned())
    }
}

fn saved_response(saved: SavedVoiceConfiguration) -> ManagedVoiceConfigurationResponse {
    saved.config.response(
        saved.id,
        saved.name,
        saved.enabled,
        ManagedVoiceConfigurationSource::Saved,
        saved.created_at,
        saved.updated_at,
    )
}

struct VolcengineVoiceBackend {
    config: VolcengineVoiceConfig,
    client: reqwest::Client,
}

impl VolcengineVoiceBackend {
    fn from_environment() -> Result<Self, ProviderConfigError> {
        let config = VolcengineVoiceConfig::from_environment()?;
        Ok(Self::from_config(config))
    }

    fn from_config(config: VolcengineVoiceConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { config, client }
    }

    async fn call(&self, action: &'static str, body: Value) -> Result<(), ProviderError> {
        let body = serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidConfiguration)?;
        let signed = sign_openapi_request(
            action,
            RTC_API_VERSION,
            &body,
            &self.config.access_key,
            &self.config.secret_key,
            Utc::now(),
        )?;
        let url = format!("{RTC_API_ENDPOINT}?Action={action}&Version={RTC_API_VERSION}");
        let response = self
            .client
            .post(url)
            .headers(signed)
            .body(body)
            .send()
            .await
            .map_err(|_| ProviderError::Transport)?;
        let status = response.status();
        let response_body = response
            .json::<Value>()
            .await
            .map_err(|_| ProviderError::InvalidResponse)?;
        let provider_error = response_body.pointer("/ResponseMetadata/Error");
        if !status.is_success() || provider_error.is_some_and(|value| !value.is_null()) {
            return Err(ProviderError::Rejected);
        }
        Ok(())
    }
}

#[async_trait]
impl ManagedVoiceBackend for VolcengineVoiceBackend {
    fn capability(&self) -> ManagedVoiceCapability {
        ManagedVoiceCapability {
            enabled: true,
            provider: Some(ManagedVoiceProvider::VolcengineRtc),
            reason: None,
        }
    }

    async fn prepare_session(
        &self,
        session: &VoiceProviderSession,
    ) -> Result<VoiceSessionRtcCredentials, ProviderError> {
        let token = serialize_rtc_token(
            &self.config.rtc_app_id,
            &self.config.rtc_app_key,
            &session.room_id,
            &session.user_id,
            (session.expires_at / 1000) as u32,
            Utc::now().timestamp() as u32,
            random_nonce(),
        )?;
        Ok(VoiceSessionRtcCredentials {
            app_id: self.config.rtc_app_id.clone(),
            room_id: session.room_id.clone(),
            user_id: session.user_id.clone(),
            token,
        })
    }

    async fn start_session(&self, session: &VoiceProviderSession) -> Result<(), ProviderError> {
        let body = self.config.start_body(session)?;
        self.call("StartVoiceChat", body).await
    }

    async fn stop_session(&self, session: &VoiceProviderSession) -> Result<(), ProviderError> {
        self.call(
            "StopVoiceChat",
            json!({
                "AppId": self.config.rtc_app_id,
                "RoomId": session.room_id,
                "TaskId": session.task_id,
            }),
        )
        .await
    }

    async fn interrupt_session(&self, session: &VoiceProviderSession) -> Result<(), ProviderError> {
        self.call(
            "UpdateVoiceChat",
            json!({
                "AppId": self.config.rtc_app_id,
                "RoomId": session.room_id,
                "TaskId": session.task_id,
                "Command": "interrupt",
            }),
        )
        .await
    }

    async fn speak_text(&self, session: &VoiceProviderSession, text: &str) -> Result<(), ProviderError> {
        let chunks = split_speech_chunks(text, 200);
        if chunks.is_empty() {
            return Err(ProviderError::InvalidConfiguration);
        }
        for (index, chunk) in chunks.iter().enumerate() {
            self.call(
                "UpdateVoiceChat",
                json!({
                    "AppId": self.config.rtc_app_id,
                    "RoomId": session.room_id,
                    "TaskId": session.task_id,
                    "Command": "ExternalTextToSpeech",
                    "Message": chunk,
                    "InterruptMode": if index == 0 { 1 } else { 2 },
                }),
            )
            .await?;
        }
        Ok(())
    }
}

fn split_speech_chunks(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.trim().chars() {
        current.push(ch);
        if current.chars().count() >= max_chars {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn random_nonce() -> u32 {
    let uuid = uuid::Uuid::now_v7();
    u32::from_le_bytes(uuid.as_bytes()[12..16].try_into().expect("UUID tail is four bytes"))
}

fn sign_openapi_request(
    action: &str,
    version: &str,
    body: &[u8],
    access_key: &str,
    secret_key: &str,
    now: DateTime<Utc>,
) -> Result<HeaderMap, ProviderError> {
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &datetime[..8];
    let body_hash = hex::encode(Sha256::digest(body));
    let canonical_query = format!("Action={action}&Version={version}");
    let canonical_headers = format!("host:{RTC_API_HOST}\nx-content-sha256:{body_hash}\nx-date:{datetime}");
    let signed_headers = "host;x-content-sha256;x-date";
    let canonical_request = format!("POST\n/\n{canonical_query}\n{canonical_headers}\n\n{signed_headers}\n{body_hash}");
    let scope = format!("{date}/{RTC_REGION}/{RTC_SERVICE}/request");
    let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = format!("HMAC-SHA256\n{datetime}\n{scope}\n{canonical_hash}");

    let date_key = hmac_bytes(secret_key.as_bytes(), date.as_bytes())?;
    let region_key = hmac_bytes(&date_key, RTC_REGION.as_bytes())?;
    let service_key = hmac_bytes(&region_key, RTC_SERVICE.as_bytes())?;
    let signing_key = hmac_bytes(&service_key, b"request")?;
    let signature = hex::encode(hmac_bytes(&signing_key, string_to_sign.as_bytes())?);
    let authorization =
        format!("HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}");

    let mut headers = HeaderMap::new();
    headers.insert(HOST, HeaderValue::from_static(RTC_API_HOST));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "x-date",
        HeaderValue::from_str(&datetime).map_err(|_| ProviderError::InvalidConfiguration)?,
    );
    headers.insert(
        "x-content-sha256",
        HeaderValue::from_str(&body_hash).map_err(|_| ProviderError::InvalidConfiguration)?,
    );
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&authorization).map_err(|_| ProviderError::InvalidConfiguration)?,
    );
    Ok(headers)
}

fn hmac_bytes(key: &[u8], message: &[u8]) -> Result<Vec<u8>, ProviderError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| ProviderError::InvalidConfiguration)?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn serialize_rtc_token(
    app_id: &str,
    app_key: &str,
    room_id: &str,
    user_id: &str,
    expires_at: u32,
    issued_at: u32,
    nonce: u32,
) -> Result<String, ProviderError> {
    if app_id.len() != 24 {
        return Err(ProviderError::InvalidConfiguration);
    }
    let mut message = Vec::new();
    put_u32(&mut message, nonce);
    put_u32(&mut message, issued_at);
    put_u32(&mut message, expires_at);
    put_bytes(&mut message, room_id.as_bytes())?;
    put_bytes(&mut message, user_id.as_bytes())?;
    put_u16(&mut message, 5);
    for privilege in 0_u16..=4 {
        put_u16(&mut message, privilege);
        put_u32(&mut message, 0);
    }

    let signature = hmac_bytes(app_key.as_bytes(), &message)?;
    let mut content = Vec::new();
    put_bytes(&mut content, &message)?;
    put_bytes(&mut content, &signature)?;
    Ok(format!(
        "001{app_id}{}",
        base64::engine::general_purpose::STANDARD.encode(content)
    ))
}

fn put_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(buffer: &mut Vec<u8>, value: &[u8]) -> Result<(), ProviderError> {
    let length = u16::try_from(value.len()).map_err(|_| ProviderError::InvalidConfiguration)?;
    put_u16(buffer, length);
    buffer.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use aionui_db::{DbError, models::VoiceConfigurationRow};
    use chrono::TimeZone;

    use super::*;

    #[derive(Default)]
    struct MemoryVoiceConfigurationRepository {
        rows: Mutex<HashMap<String, VoiceConfigurationRow>>,
    }

    #[async_trait]
    impl IVoiceConfigurationRepository for MemoryVoiceConfigurationRepository {
        async fn get(&self, user_id: &str) -> Result<Option<VoiceConfigurationRow>, DbError> {
            Ok(self.rows.lock().unwrap().get(user_id).cloned())
        }

        async fn upsert(&self, user_id: &str, configuration_encrypted: &str) -> Result<VoiceConfigurationRow, DbError> {
            let row = VoiceConfigurationRow {
                user_id: user_id.to_owned(),
                configuration_encrypted: configuration_encrypted.to_owned(),
                updated_at: aionui_common::now_ms(),
            };
            self.rows.lock().unwrap().insert(user_id.to_owned(), row.clone());
            Ok(row)
        }
    }

    fn valid_update() -> UpdateManagedVoiceConfigurationRequest {
        UpdateManagedVoiceConfigurationRequest {
            name: "Production voice".to_owned(),
            enabled: true,
            rtc_app_id: "123456789012345678901234".to_owned(),
            access_key: Some("secret-access-key".to_owned()),
            secret_key: Some("secret-signing-key".to_owned()),
            rtc_app_key: Some("secret-rtc-app-key".to_owned()),
            agent_user_id: "voice-agent".to_owned(),
            welcome_message: "hello".to_owned(),
            asr_app_id: "asr-app".to_owned(),
            asr_cluster: "asr-cluster".to_owned(),
            tts_app_id: "tts-app".to_owned(),
            tts_cluster: "tts-cluster".to_owned(),
            tts_voice_type: "tts-voice".to_owned(),
            llm_url: "https://example.com".to_owned(),
            llm_api_key: Some("secret-llm-key".to_owned()),
            llm_model_name: "voice-model".to_owned(),
            system_message: "be concise".to_owned(),
        }
    }

    fn valid_values() -> HashMap<&'static str, String> {
        HashMap::from([
            (ACCESS_KEY_ENV, "ak-test".to_owned()),
            (SECRET_KEY_ENV, "sk-test".to_owned()),
            (RTC_APP_ID_ENV, "123456789012345678901234".to_owned()),
            (RTC_APP_KEY_ENV, "rtc-app-key".to_owned()),
            (
                VOICE_CHAT_CONFIG_ENV,
                r#"{"AgentConfig":{"UserId":"voice-agent"},"Config":{"InterruptMode":0}}"#.to_owned(),
            ),
        ])
    }

    #[test]
    fn missing_configuration_fails_closed() {
        let result = VolcengineVoiceConfig::from_lookup(|_| None);
        assert!(matches!(result, Err(ProviderConfigError::Missing)));
    }

    #[test]
    fn empty_agent_user_id_fails_closed() {
        let mut values = valid_values();
        values.insert(
            VOICE_CHAT_CONFIG_ENV,
            r#"{"AgentConfig":{"UserId":""},"Config":{"InterruptMode":0}}"#.to_owned(),
        );
        let result = VolcengineVoiceConfig::from_lookup(|name| values.get(name).cloned());
        assert!(matches!(result, Err(ProviderConfigError::Invalid)));
    }

    #[test]
    fn start_body_overrides_session_bound_fields() {
        let values = valid_values();
        let config = VolcengineVoiceConfig::from_lookup(|name| values.get(name).cloned()).unwrap();
        let body = config
            .start_body(&VoiceProviderSession {
                session_id: "voice-1".to_owned(),
                room_id: "room-1".to_owned(),
                user_id: "user-1".to_owned(),
                task_id: "task-1".to_owned(),
                expires_at: 0,
                mode: ManagedVoiceSessionMode::Conversation,
            })
            .unwrap();
        assert_eq!(body["AppId"], "123456789012345678901234");
        assert_eq!(body["RoomId"], "room-1");
        assert_eq!(body["TaskId"], "task-1");
        assert_eq!(body["AgentConfig"]["TargetUserId"], json!(["user-1"]));
        assert_eq!(body["AgentConfig"]["UserId"], "voice-agent");
    }

    #[test]
    fn dictation_start_body_suppresses_the_conversation_greeting() {
        let values = valid_values();
        let config = VolcengineVoiceConfig::from_lookup(|name| values.get(name).cloned()).unwrap();
        let body = config
            .start_body(&VoiceProviderSession {
                session_id: "voice-1".to_owned(),
                room_id: "room-1".to_owned(),
                user_id: "user-1".to_owned(),
                task_id: "task-1".to_owned(),
                expires_at: 0,
                mode: ManagedVoiceSessionMode::Dictation,
            })
            .unwrap();

        assert!(body["AgentConfig"].get("WelcomeMessage").is_none());
    }

    #[test]
    fn rtc_token_matches_official_demo_wire_format() {
        let token = serialize_rtc_token(
            "123456789012345678901234",
            "rtc-app-key",
            "room-1",
            "user-1",
            1_800_000_000,
            1_700_000_000,
            0x1234_5678,
        )
        .unwrap();
        assert_eq!(
            token,
            "001123456789012345678901234PAB4VjQSAPFTZQDSSWsGAHJvb20tMQYAdXNlci0xBQAAAAAAAAABAAAAAAACAAAAAAADAAAAAAAEAAAAAAAgAChsCgUWWvyIAoz4q6d2Jq4rCI4XvOh6sRhLAVQntui8"
        );
    }

    #[test]
    fn openapi_signature_is_stable_for_fixed_input() {
        let now = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
        let headers = sign_openapi_request(
            "StartVoiceChat",
            RTC_API_VERSION,
            br#"{"AppId":"123"}"#,
            "ak-test",
            "sk-test",
            now,
        )
        .unwrap();
        assert_eq!(headers["x-date"], "20240102T030405Z");
        assert_eq!(
            headers[AUTHORIZATION],
            "HMAC-SHA256 Credential=ak-test/20240102/cn-north-1/rtc/request, SignedHeaders=host;x-content-sha256;x-date, Signature=ef0ca171899891776b2ebaed12c40d1f9bc6e79b0e1243a561a6d2bfcc944fe4"
        );
    }

    #[tokio::test]
    async fn saved_configuration_is_encrypted_redacted_and_scoped_per_user() {
        let repository = Arc::new(MemoryVoiceConfigurationRepository::default());
        let encryption_key = [7_u8; 32];
        let registry = VoiceProviderRegistry::with_environment(repository.clone(), encryption_key, None);

        let response = registry.create("user-a", valid_update()).await.unwrap();
        assert!(response.access_key_configured);
        assert!(response.secret_key_configured);
        assert!(response.rtc_app_key_configured);
        assert!(response.llm_api_key_configured);

        let stored = repository.get("user-a").await.unwrap().unwrap();
        assert!(!stored.configuration_encrypted.contains("secret-access-key"));
        assert!(!stored.configuration_encrypted.contains("secret-signing-key"));
        assert!(!stored.configuration_encrypted.contains("secret-rtc-app-key"));
        assert!(!stored.configuration_encrypted.contains("secret-llm-key"));

        let other_user = registry.configurations("user-b").await.unwrap();
        assert!(other_user.is_empty());
    }

    #[tokio::test]
    async fn unreadable_saved_catalog_fails_closed_without_overwriting_it() {
        let repository = Arc::new(MemoryVoiceConfigurationRepository::default());
        repository.rows.lock().unwrap().insert(
            "user-a".to_owned(),
            VoiceConfigurationRow {
                user_id: "user-a".to_owned(),
                configuration_encrypted: "corrupted-ciphertext".to_owned(),
                updated_at: aionui_common::now_ms(),
            },
        );
        let values = valid_values();
        let environment = VolcengineVoiceConfig::from_lookup(|name| values.get(name).cloned()).unwrap();
        let registry = VoiceProviderRegistry::with_environment(repository.clone(), [17_u8; 32], Some(environment));

        assert!(registry.create("user-a", valid_update()).await.is_err());
        assert_eq!(
            repository.get("user-a").await.unwrap().unwrap().configuration_encrypted,
            "corrupted-ciphertext"
        );
        let capability = registry.capability("user-a").await;
        assert!(!capability.enabled);
        assert_eq!(capability.reason.as_deref(), Some("configuration_unavailable"));
    }

    #[tokio::test]
    async fn blank_secret_patch_preserves_existing_credentials() {
        let repository = Arc::new(MemoryVoiceConfigurationRepository::default());
        let encryption_key = [9_u8; 32];
        let registry = VoiceProviderRegistry::with_environment(repository.clone(), encryption_key, None);
        let created = registry.create("user-a", valid_update()).await.unwrap();

        let mut patch = valid_update();
        patch.welcome_message = "updated hello".to_owned();
        patch.access_key = None;
        patch.secret_key = Some(String::new());
        patch.rtc_app_key = None;
        patch.llm_api_key = None;
        let response = registry.update("user-a", &created.id, patch).await.unwrap();
        assert_eq!(response.welcome_message, "updated hello");

        let stored = repository.get("user-a").await.unwrap().unwrap();
        let plaintext = decrypt_string(&stored.configuration_encrypted, &encryption_key).unwrap();
        assert!(plaintext.contains("secret-access-key"));
        assert!(plaintext.contains("secret-signing-key"));
        assert!(plaintext.contains("secret-rtc-app-key"));
        assert!(plaintext.contains("secret-llm-key"));
    }

    #[tokio::test]
    async fn enabling_configuration_disables_the_previous_active_one() {
        let repository = Arc::new(MemoryVoiceConfigurationRepository::default());
        let registry = VoiceProviderRegistry::with_environment(repository, [11_u8; 32], None);
        let first = registry.create("user-a", valid_update()).await.unwrap();
        let mut second_request = valid_update();
        second_request.name = "Backup voice".to_owned();
        second_request.enabled = false;
        let second = registry.create("user-a", second_request).await.unwrap();

        registry.set_enabled("user-a", &second.id, true).await.unwrap();
        let configurations = registry.configurations("user-a").await.unwrap();

        assert!(!configurations.iter().find(|item| item.id == first.id).unwrap().enabled);
        assert!(configurations.iter().find(|item| item.id == second.id).unwrap().enabled);
    }

    #[tokio::test]
    async fn environment_configuration_is_read_only_and_remains_the_fallback() {
        let values = valid_values();
        let environment = VolcengineVoiceConfig::from_lookup(|name| values.get(name).cloned()).unwrap();
        let repository = Arc::new(MemoryVoiceConfigurationRepository::default());
        let registry = VoiceProviderRegistry::with_environment(repository, [13_u8; 32], Some(environment));

        assert!(matches!(
            registry.update("user-a", "environment", valid_update()).await,
            Err(ProviderError::Managed)
        ));
        assert!(matches!(
            registry.set_enabled("user-a", "environment", false).await,
            Err(ProviderError::Managed)
        ));
        assert!(matches!(
            registry.delete("user-a", "environment").await,
            Err(ProviderError::Managed)
        ));

        let mut request = valid_update();
        request.enabled = false;
        let saved = registry.create("user-a", request).await.unwrap();
        let configurations = registry.configurations("user-a").await.unwrap();

        assert_eq!(configurations.len(), 2);
        assert!(
            configurations
                .iter()
                .find(|item| item.id == "environment")
                .unwrap()
                .enabled
        );
        assert!(!configurations.iter().find(|item| item.id == saved.id).unwrap().enabled);
        assert_eq!(
            registry.resolved("user-a").await.unwrap().unwrap().1,
            ManagedVoiceConfigurationSource::Environment
        );
    }
}
