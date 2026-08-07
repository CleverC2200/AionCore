use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::{
    ManagedVoiceCapability, ManagedVoiceConfigurationResponse, ManagedVoiceHealthResponse,
    UpdateManagedVoiceConfigurationRequest, VoiceSessionCreateRequest, VoiceSessionCreateResponse, VoiceTurnRequest,
    VoiceTurnResponse,
};
use aionui_common::{generate_prefixed_id, now_ms};
use tokio::sync::Mutex;

use crate::VoiceConversationAgent;
use crate::error::VoiceError;
use crate::provider::{ManagedVoiceBackend, ProviderError, VoiceProviderRegistry, VoiceProviderSession};

const SESSION_LIFETIME_MS: i64 = 60 * 60 * 1000;
const ENDED_SESSION_RETENTION_MS: i64 = 5 * 60 * 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Prepared,
    Starting,
    Active,
    StopRequested,
    Stopping,
    Ended,
}

#[derive(Clone)]
struct SessionRecord {
    owner_user_id: String,
    conversation_id: Option<String>,
    provider_session: VoiceProviderSession,
    provider: Arc<dyn ManagedVoiceBackend>,
    status: SessionStatus,
    turn_in_flight: bool,
    ended_at: Option<i64>,
}

pub struct VoiceService {
    provider: Option<Arc<dyn ManagedVoiceBackend>>,
    provider_registry: Option<Arc<VoiceProviderRegistry>>,
    agent: Arc<dyn VoiceConversationAgent>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
}

impl VoiceService {
    pub fn new(provider: Arc<dyn ManagedVoiceBackend>, agent: Arc<dyn VoiceConversationAgent>) -> Self {
        Self {
            provider: Some(provider),
            provider_registry: None,
            agent,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_registry(registry: Arc<VoiceProviderRegistry>, agent: Arc<dyn VoiceConversationAgent>) -> Self {
        Self {
            provider: None,
            provider_registry: Some(registry),
            agent,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    async fn provider_for(&self, owner_user_id: &str) -> Arc<dyn ManagedVoiceBackend> {
        if let Some(registry) = &self.provider_registry {
            registry.backend(owner_user_id).await
        } else {
            self.provider.as_ref().expect("voice provider is configured").clone()
        }
    }

    pub async fn capability(&self, owner_user_id: &str) -> ManagedVoiceCapability {
        if let Some(registry) = &self.provider_registry {
            registry.capability(owner_user_id).await
        } else {
            self.provider
                .as_ref()
                .expect("voice provider is configured")
                .capability()
        }
    }

    fn registry(&self) -> Result<&Arc<VoiceProviderRegistry>, VoiceError> {
        self.provider_registry.as_ref().ok_or(VoiceError::InvalidConfiguration)
    }

    pub async fn configurations(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<ManagedVoiceConfigurationResponse>, VoiceError> {
        self.registry()?
            .configurations(owner_user_id)
            .await
            .map_err(provider_configuration_error)
    }

    pub async fn create_configuration(
        &self,
        owner_user_id: &str,
        request: UpdateManagedVoiceConfigurationRequest,
    ) -> Result<ManagedVoiceConfigurationResponse, VoiceError> {
        self.registry()?
            .create(owner_user_id, request)
            .await
            .map_err(provider_configuration_error)
    }

    pub async fn update_configuration(
        &self,
        owner_user_id: &str,
        configuration_id: &str,
        request: UpdateManagedVoiceConfigurationRequest,
    ) -> Result<ManagedVoiceConfigurationResponse, VoiceError> {
        self.registry()?
            .update(owner_user_id, configuration_id, request)
            .await
            .map_err(provider_configuration_error)
    }

    pub async fn set_configuration_enabled(
        &self,
        owner_user_id: &str,
        configuration_id: &str,
        enabled: bool,
    ) -> Result<ManagedVoiceConfigurationResponse, VoiceError> {
        self.registry()?
            .set_enabled(owner_user_id, configuration_id, enabled)
            .await
            .map_err(provider_configuration_error)
    }

    pub async fn delete_configuration(&self, owner_user_id: &str, configuration_id: &str) -> Result<(), VoiceError> {
        self.registry()?
            .delete(owner_user_id, configuration_id)
            .await
            .map_err(provider_configuration_error)
    }

    pub async fn configuration_health(
        &self,
        owner_user_id: &str,
        configuration_id: &str,
    ) -> Result<ManagedVoiceHealthResponse, VoiceError> {
        self.registry()?
            .health(owner_user_id, configuration_id)
            .await
            .map_err(provider_configuration_error)
    }

    pub async fn create_session(
        &self,
        owner_user_id: &str,
        request: VoiceSessionCreateRequest,
    ) -> Result<VoiceSessionCreateResponse, VoiceError> {
        self.cleanup_expired_sessions(owner_user_id).await;
        let provider = self.provider_for(owner_user_id).await;
        if !provider.capability().enabled {
            return Err(VoiceError::Disabled);
        }

        let session_id = generate_prefixed_id("voice");
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let provider_session = VoiceProviderSession {
            session_id: session_id.clone(),
            room_id: format!("aion-voice-{suffix}"),
            user_id: format!("user-{suffix}"),
            task_id: session_id.clone(),
            expires_at: now_ms() + SESSION_LIFETIME_MS,
            mode: request.mode,
        };

        {
            let mut sessions = self.sessions.lock().await;
            if sessions.values().any(|record| {
                record.owner_user_id == owner_user_id
                    && matches!(
                        record.status,
                        SessionStatus::Prepared
                            | SessionStatus::Starting
                            | SessionStatus::Active
                            | SessionStatus::StopRequested
                            | SessionStatus::Stopping
                    )
            }) {
                return Err(VoiceError::AlreadyActive);
            }
            sessions.insert(
                session_id.clone(),
                SessionRecord {
                    owner_user_id: owner_user_id.to_owned(),
                    conversation_id: request.conversation_id,
                    provider_session: provider_session.clone(),
                    provider: provider.clone(),
                    status: SessionStatus::Prepared,
                    turn_in_flight: false,
                    ended_at: None,
                },
            );
        }

        let rtc = match provider.prepare_session(&provider_session).await {
            Ok(rtc) => rtc,
            Err(error) => {
                self.sessions.lock().await.remove(&session_id);
                tracing::warn!(
                    session_id,
                    error_kind = error.kind(),
                    "managed voice session preparation failed"
                );
                return Err(VoiceError::ProviderUnavailable);
            }
        };

        tracing::info!(session_id, "managed voice session prepared");

        Ok(VoiceSessionCreateResponse {
            session_id,
            rtc,
            expires_at: provider_session.expires_at,
        })
    }

    pub async fn start_session(&self, owner_user_id: &str, session_id: &str) -> Result<(), VoiceError> {
        let (provider_session, provider) = {
            let mut sessions = self.sessions.lock().await;
            let Some(record) = sessions
                .get_mut(session_id)
                .filter(|record| record.owner_user_id == owner_user_id)
            else {
                return Err(VoiceError::SessionNotFound);
            };
            if record.provider_session.expires_at <= now_ms() {
                record.status = SessionStatus::Ended;
                record.ended_at = Some(now_ms());
                return Err(VoiceError::SessionNotFound);
            }
            match record.status {
                SessionStatus::Prepared => {
                    record.status = SessionStatus::Starting;
                    (record.provider_session.clone(), record.provider.clone())
                }
                SessionStatus::Starting | SessionStatus::Active => return Ok(()),
                SessionStatus::StopRequested | SessionStatus::Stopping | SessionStatus::Ended => {
                    return Err(VoiceError::SessionNotFound);
                }
            }
        };

        if let Err(error) = provider.start_session(&provider_session).await {
            if let Some(record) = self.sessions.lock().await.get_mut(session_id) {
                record.status = if record.status == SessionStatus::StopRequested {
                    record.ended_at = Some(now_ms());
                    SessionStatus::Ended
                } else {
                    SessionStatus::Prepared
                };
            }
            tracing::warn!(
                session_id,
                error_kind = error.kind(),
                "managed voice agent start failed"
            );
            return Err(VoiceError::ProviderUnavailable);
        }

        let stop_requested = {
            let mut sessions = self.sessions.lock().await;
            let Some(record) = sessions.get_mut(session_id) else {
                return Err(VoiceError::SessionNotFound);
            };
            if record.status == SessionStatus::StopRequested {
                record.status = SessionStatus::Stopping;
                true
            } else {
                record.status = SessionStatus::Active;
                false
            }
        };

        if stop_requested {
            self.stop_provider_session(session_id, &provider_session, &provider)
                .await?;
        } else {
            tracing::info!(session_id, "managed voice agent started");
        }
        Ok(())
    }

    pub async fn stop_session(&self, owner_user_id: &str, session_id: &str) -> Result<(), VoiceError> {
        let (provider_session, provider) = {
            let mut sessions = self.sessions.lock().await;
            let Some(record) = sessions
                .get_mut(session_id)
                .filter(|record| record.owner_user_id == owner_user_id)
            else {
                return Err(VoiceError::SessionNotFound);
            };
            match record.status {
                SessionStatus::Prepared => {
                    record.status = SessionStatus::Ended;
                    record.ended_at = Some(now_ms());
                    tracing::info!(session_id, "prepared managed voice session discarded");
                    return Ok(());
                }
                SessionStatus::Starting => {
                    record.status = SessionStatus::StopRequested;
                    return Ok(());
                }
                SessionStatus::StopRequested | SessionStatus::Ended | SessionStatus::Stopping => return Ok(()),
                SessionStatus::Active => {
                    record.status = SessionStatus::Stopping;
                    (record.provider_session.clone(), record.provider.clone())
                }
            }
        };

        self.stop_provider_session(session_id, &provider_session, &provider)
            .await
    }

    async fn stop_provider_session(
        &self,
        session_id: &str,
        provider_session: &VoiceProviderSession,
        provider: &Arc<dyn ManagedVoiceBackend>,
    ) -> Result<(), VoiceError> {
        if let Err(error) = provider.stop_session(provider_session).await {
            if let Some(record) = self.sessions.lock().await.get_mut(session_id) {
                record.status = SessionStatus::Active;
            }
            tracing::warn!(
                session_id,
                error_kind = error.kind(),
                "managed voice session stop failed"
            );
            return Err(VoiceError::ProviderUnavailable);
        }

        if let Some(record) = self.sessions.lock().await.get_mut(session_id) {
            record.status = SessionStatus::Ended;
            record.ended_at = Some(now_ms());
        }
        tracing::info!(session_id, "managed voice session stopped");
        Ok(())
    }

    async fn cleanup_expired_sessions(&self, owner_user_id: &str) {
        let now = now_ms();
        let expired = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, record| {
                !matches!(record.status, SessionStatus::Ended)
                    || record
                        .ended_at
                        .is_none_or(|ended_at| now - ended_at <= ENDED_SESSION_RETENTION_MS)
            });
            sessions
                .iter()
                .filter(|(_, record)| {
                    record.owner_user_id == owner_user_id
                        && record.provider_session.expires_at <= now
                        && !matches!(record.status, SessionStatus::Ended)
                })
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>()
        };
        for session_id in expired {
            if let Err(error) = self.stop_session(owner_user_id, &session_id).await {
                tracing::warn!(session_id, error = %error, "failed to stop expired managed voice session");
            }
        }
    }

    pub async fn stop_sessions_for_user(&self, owner_user_id: &str) {
        let session_ids = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .filter(|(_, record)| {
                    record.owner_user_id == owner_user_id
                        && matches!(
                            record.status,
                            SessionStatus::Prepared
                                | SessionStatus::Starting
                                | SessionStatus::Active
                                | SessionStatus::StopRequested
                        )
                })
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>()
        };

        for session_id in session_ids {
            if let Err(error) = self.stop_session(owner_user_id, &session_id).await {
                tracing::warn!(session_id, error = %error, "failed to stop managed voice session for revoked user");
            }
        }
    }

    pub async fn run_turn(
        &self,
        owner_user_id: &str,
        session_id: &str,
        request: VoiceTurnRequest,
    ) -> Result<VoiceTurnResponse, VoiceError> {
        let text = request.text.trim();
        if text.is_empty() || text.chars().count() > 10_000 {
            return Err(VoiceError::InvalidTranscript);
        }

        let (conversation_id, provider_session, provider) = {
            let mut sessions = self.sessions.lock().await;
            let Some(record) = sessions
                .get_mut(session_id)
                .filter(|record| record.owner_user_id == owner_user_id && record.status == SessionStatus::Active)
            else {
                return Err(VoiceError::SessionNotFound);
            };
            if record.turn_in_flight {
                return Err(VoiceError::TurnBusy);
            }
            let conversation_id = record
                .conversation_id
                .clone()
                .filter(|value| !value.trim().is_empty())
                .ok_or(VoiceError::ConversationRequired)?;
            record.turn_in_flight = true;
            (
                conversation_id,
                record.provider_session.clone(),
                record.provider.clone(),
            )
        };

        let result = self
            .run_turn_inner(
                owner_user_id,
                session_id,
                &conversation_id,
                &provider_session,
                &provider,
                text,
            )
            .await;
        if let Some(record) = self.sessions.lock().await.get_mut(session_id) {
            record.turn_in_flight = false;
        }
        result
    }

    async fn run_turn_inner(
        &self,
        owner_user_id: &str,
        session_id: &str,
        conversation_id: &str,
        provider_session: &VoiceProviderSession,
        provider: &Arc<dyn ManagedVoiceBackend>,
        text: &str,
    ) -> Result<VoiceTurnResponse, VoiceError> {
        provider.interrupt_session(provider_session).await.map_err(|error| {
            tracing::warn!(session_id, error_kind = error.kind(), "managed voice interrupt failed");
            VoiceError::ProviderUnavailable
        })?;

        tracing::info!(
            session_id,
            conversation_id,
            transcript_chars = text.chars().count(),
            "voice turn forwarded"
        );
        let response = self
            .agent
            .respond(owner_user_id, conversation_id, text)
            .await
            .map_err(|error| {
                tracing::warn!(
                    session_id,
                    conversation_id,
                    error_kind = error.kind(),
                    "voice agent turn failed"
                );
                VoiceError::AgentUnavailable
            })?;

        {
            let sessions = self.sessions.lock().await;
            let active = sessions
                .get(session_id)
                .is_some_and(|record| record.owner_user_id == owner_user_id && record.status == SessionStatus::Active);
            if !active {
                return Err(VoiceError::SessionNotFound);
            }
        }

        provider.interrupt_session(provider_session).await.map_err(|error| {
            tracing::warn!(
                session_id,
                error_kind = error.kind(),
                "managed voice playback interrupt failed"
            );
            VoiceError::ProviderUnavailable
        })?;
        provider
            .speak_text(provider_session, &response)
            .await
            .map_err(|error| {
                tracing::warn!(session_id, error_kind = error.kind(), "managed voice playback failed");
                VoiceError::ProviderUnavailable
            })?;
        tracing::info!(
            session_id,
            conversation_id,
            response_chars = response.chars().count(),
            "voice turn playback queued"
        );
        Ok(VoiceTurnResponse { text: response })
    }
}

fn provider_configuration_error(error: ProviderError) -> VoiceError {
    match error {
        ProviderError::NotFound => VoiceError::ConfigurationNotFound,
        ProviderError::Managed => VoiceError::ConfigurationManaged,
        ProviderError::Storage => VoiceError::ConfigurationUnavailable,
        _ => VoiceError::InvalidConfiguration,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aionui_api_types::{ManagedVoiceCapability, ManagedVoiceProvider, VoiceSessionRtcCredentials};
    use async_trait::async_trait;

    use crate::provider::ProviderError;

    use super::*;

    struct MockBackend {
        enabled: bool,
        prepares: AtomicUsize,
        starts: AtomicUsize,
        stops: AtomicUsize,
        stop_failures: AtomicUsize,
        interrupts: AtomicUsize,
        speaks: AtomicUsize,
    }

    impl MockBackend {
        fn enabled() -> Self {
            Self {
                enabled: true,
                prepares: AtomicUsize::new(0),
                starts: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
                stop_failures: AtomicUsize::new(0),
                interrupts: AtomicUsize::new(0),
                speaks: AtomicUsize::new(0),
            }
        }
    }

    struct MockAgent {
        calls: AtomicUsize,
    }

    impl MockAgent {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl VoiceConversationAgent for MockAgent {
        async fn respond(
            &self,
            _owner_user_id: &str,
            _conversation_id: &str,
            _text: &str,
        ) -> Result<String, crate::VoiceAgentError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("客户端 Agent 回复".to_owned())
        }
    }

    #[async_trait]
    impl ManagedVoiceBackend for MockBackend {
        fn capability(&self) -> ManagedVoiceCapability {
            ManagedVoiceCapability {
                enabled: self.enabled,
                provider: Some(ManagedVoiceProvider::VolcengineRtc),
                reason: (!self.enabled).then(|| "not_configured".to_owned()),
            }
        }

        async fn prepare_session(
            &self,
            session: &VoiceProviderSession,
        ) -> Result<VoiceSessionRtcCredentials, ProviderError> {
            self.prepares.fetch_add(1, Ordering::Relaxed);
            Ok(VoiceSessionRtcCredentials {
                app_id: "123456789012345678901234".to_owned(),
                room_id: session.room_id.clone(),
                user_id: session.user_id.clone(),
                token: "temporary-token".to_owned(),
            })
        }

        async fn start_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn stop_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
            self.stops.fetch_add(1, Ordering::Relaxed);
            if self
                .stop_failures
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    if remaining > 0 { Some(remaining - 1) } else { None }
                })
                .is_ok()
            {
                return Err(ProviderError::Transport);
            }
            Ok(())
        }

        async fn interrupt_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
            self.interrupts.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn speak_text(&self, _session: &VoiceProviderSession, _text: &str) -> Result<(), ProviderError> {
            self.speaks.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn disabled_backend_fails_closed() {
        let backend = Arc::new(MockBackend {
            enabled: false,
            prepares: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            stop_failures: AtomicUsize::new(0),
            interrupts: AtomicUsize::new(0),
            speaks: AtomicUsize::new(0),
        });
        let service = VoiceService::new(backend.clone(), Arc::new(MockAgent::new()));
        let result = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await;
        assert!(matches!(result, Err(VoiceError::Disabled)));
        assert_eq!(backend.prepares.load(Ordering::Relaxed), 0);
        assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn one_active_session_per_user_and_stop_is_idempotent() {
        let backend = Arc::new(MockBackend::enabled());
        let service = VoiceService::new(backend.clone(), Arc::new(MockAgent::new()));
        let created = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();
        let duplicate = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await;
        assert!(matches!(duplicate, Err(VoiceError::AlreadyActive)));

        service.start_session("user-1", &created.session_id).await.unwrap();
        service.start_session("user-1", &created.session_id).await.unwrap();
        service.stop_session("user-1", &created.session_id).await.unwrap();
        service.stop_session("user-1", &created.session_id).await.unwrap();
        assert_eq!(backend.starts.load(Ordering::Relaxed), 1);
        assert_eq!(backend.stops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn failed_provider_stop_can_be_retried() {
        let backend = Arc::new(MockBackend::enabled());
        backend.stop_failures.store(1, Ordering::Relaxed);
        let service = VoiceService::new(backend.clone(), Arc::new(MockAgent::new()));
        let created = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();
        service.start_session("user-1", &created.session_id).await.unwrap();

        assert!(matches!(
            service.stop_session("user-1", &created.session_id).await,
            Err(VoiceError::ProviderUnavailable)
        ));
        service.stop_session("user-1", &created.session_id).await.unwrap();
        assert_eq!(backend.stops.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn prepared_session_can_be_discarded_without_starting_provider() {
        let backend = Arc::new(MockBackend::enabled());
        let service = VoiceService::new(backend.clone(), Arc::new(MockAgent::new()));
        let created = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();

        service.stop_session("user-1", &created.session_id).await.unwrap();
        assert_eq!(backend.prepares.load(Ordering::Relaxed), 1);
        assert_eq!(backend.starts.load(Ordering::Relaxed), 0);
        assert_eq!(backend.stops.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn expired_prepared_session_does_not_block_a_new_session() {
        let backend = Arc::new(MockBackend::enabled());
        let service = VoiceService::new(backend, Arc::new(MockAgent::new()));
        let created = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();
        service
            .sessions
            .lock()
            .await
            .get_mut(&created.session_id)
            .unwrap()
            .provider_session
            .expires_at = now_ms() - 1;

        let replacement = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await;
        assert!(replacement.is_ok());
    }

    #[tokio::test]
    async fn old_ended_session_tombstones_are_pruned() {
        let backend = Arc::new(MockBackend::enabled());
        let service = VoiceService::new(backend, Arc::new(MockAgent::new()));
        let created = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();
        service.stop_session("user-1", &created.session_id).await.unwrap();
        service
            .sessions
            .lock()
            .await
            .get_mut(&created.session_id)
            .unwrap()
            .ended_at = Some(now_ms() - ENDED_SESSION_RETENTION_MS - 1);

        service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();
        assert!(!service.sessions.lock().await.contains_key(&created.session_id));
    }

    #[tokio::test]
    async fn session_ownership_is_not_disclosed() {
        let backend = Arc::new(MockBackend::enabled());
        let service = VoiceService::new(backend.clone(), Arc::new(MockAgent::new()));
        let created = service
            .create_session("user-1", VoiceSessionCreateRequest::default())
            .await
            .unwrap();

        let result = service.stop_session("user-2", &created.session_id).await;
        assert!(matches!(result, Err(VoiceError::SessionNotFound)));
        assert_eq!(backend.stops.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn voice_turn_uses_bound_conversation_agent_and_provider_tts() {
        let backend = Arc::new(MockBackend::enabled());
        let agent = Arc::new(MockAgent::new());
        let service = VoiceService::new(backend.clone(), agent.clone());
        let created = service
            .create_session(
                "user-1",
                VoiceSessionCreateRequest {
                    conversation_id: Some("conversation-1".to_owned()),
                    ..VoiceSessionCreateRequest::default()
                },
            )
            .await
            .unwrap();
        service.start_session("user-1", &created.session_id).await.unwrap();

        let response = service
            .run_turn(
                "user-1",
                &created.session_id,
                VoiceTurnRequest {
                    text: "查询当前客户".to_owned(),
                },
            )
            .await
            .unwrap();

        assert_eq!(response.text, "客户端 Agent 回复");
        assert_eq!(agent.calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.interrupts.load(Ordering::Relaxed), 2);
        assert_eq!(backend.speaks.load(Ordering::Relaxed), 1);
    }
}
