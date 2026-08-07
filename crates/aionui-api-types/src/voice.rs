use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedVoiceProvider {
    VolcengineRtc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedVoiceCapability {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ManagedVoiceProvider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedVoiceConfigurationSource {
    Environment,
    Saved,
    NotConfigured,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedVoiceConfigurationResponse {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub provider: ManagedVoiceProvider,
    pub source: ManagedVoiceConfigurationSource,
    pub rtc_app_id: String,
    pub access_key_configured: bool,
    pub secret_key_configured: bool,
    pub rtc_app_key_configured: bool,
    pub agent_user_id: String,
    pub welcome_message: String,
    pub asr_app_id: String,
    pub asr_cluster: String,
    pub tts_app_id: String,
    pub tts_cluster: String,
    pub tts_voice_type: String,
    pub llm_url: String,
    pub llm_api_key_configured: bool,
    pub llm_model_name: String,
    pub system_message: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateManagedVoiceConfigurationRequest {
    pub name: String,
    pub enabled: bool,
    pub rtc_app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtc_app_key: Option<String>,
    pub agent_user_id: String,
    pub welcome_message: String,
    pub asr_app_id: String,
    pub asr_cluster: String,
    pub tts_app_id: String,
    pub tts_cluster: String,
    pub tts_voice_type: String,
    pub llm_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_api_key: Option<String>,
    pub llm_model_name: String,
    pub system_message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetManagedVoiceConfigurationEnabledRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedVoiceHealthStatus {
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedVoiceHealthResponse {
    pub status: ManagedVoiceHealthStatus,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedVoiceSessionMode {
    #[default]
    Conversation,
    Dictation,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSessionCreateRequest {
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub mode: ManagedVoiceSessionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSessionRtcCredentials {
    pub app_id: String,
    pub room_id: String,
    pub user_id: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceSessionCreateResponse {
    pub session_id: String,
    pub rtc: VoiceSessionRtcCredentials,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceTurnRequest {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceTurnResponse {
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_uses_frontend_contract_name() {
        let value = serde_json::to_value(ManagedVoiceProvider::VolcengineRtc).unwrap();
        assert_eq!(value, "volcengine-rtc");
    }

    #[test]
    fn capability_omits_absent_optional_fields() {
        let value = serde_json::to_value(ManagedVoiceCapability {
            enabled: false,
            provider: None,
            reason: None,
        })
        .unwrap();
        assert_eq!(value, serde_json::json!({ "enabled": false }));
    }

    #[test]
    fn session_mode_defaults_to_conversation_and_accepts_dictation() {
        let default_request: VoiceSessionCreateRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default_request.mode, ManagedVoiceSessionMode::Conversation);

        let dictation_request: VoiceSessionCreateRequest =
            serde_json::from_value(serde_json::json!({ "mode": "dictation" })).unwrap();
        assert_eq!(dictation_request.mode, ManagedVoiceSessionMode::Dictation);
    }

    #[test]
    fn voice_configuration_response_contains_only_secret_presence_flags() {
        let response = ManagedVoiceConfigurationResponse {
            id: "voice-1".to_owned(),
            name: "Production".to_owned(),
            enabled: true,
            provider: ManagedVoiceProvider::VolcengineRtc,
            source: ManagedVoiceConfigurationSource::Saved,
            rtc_app_id: "app-id".to_owned(),
            access_key_configured: true,
            secret_key_configured: true,
            rtc_app_key_configured: true,
            agent_user_id: "agent".to_owned(),
            welcome_message: String::new(),
            asr_app_id: "asr".to_owned(),
            asr_cluster: "cluster".to_owned(),
            tts_app_id: "tts".to_owned(),
            tts_cluster: "cluster".to_owned(),
            tts_voice_type: "voice".to_owned(),
            llm_url: "https://example.com".to_owned(),
            llm_api_key_configured: true,
            llm_model_name: "model".to_owned(),
            system_message: String::new(),
            created_at: 1,
            updated_at: 2,
        };

        let value = serde_json::to_value(response).unwrap();
        assert!(value.get("access_key").is_none());
        assert!(value.get("secret_key").is_none());
        assert!(value.get("rtc_app_key").is_none());
        assert!(value.get("llm_api_key").is_none());
        assert_eq!(value["access_key_configured"], true);
    }
}
