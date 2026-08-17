#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("managed voice is not configured")]
    Disabled,
    #[error("an active voice session already exists")]
    AlreadyActive,
    #[error("voice session not found")]
    SessionNotFound,
    #[error("voice provider is unavailable")]
    ProviderUnavailable,
    #[error("voice session is not bound to a conversation")]
    ConversationRequired,
    #[error("voice transcript is invalid")]
    InvalidTranscript,
    #[error("a voice turn is already running")]
    TurnBusy,
    #[error("conversation agent is unavailable")]
    AgentUnavailable,
    #[error("managed voice configuration is invalid")]
    InvalidConfiguration,
    #[error("managed voice configuration storage is unavailable")]
    ConfigurationUnavailable,
    #[error("managed voice configuration not found")]
    ConfigurationNotFound,
    #[error("managed voice configuration is read-only")]
    ConfigurationManaged,
}
