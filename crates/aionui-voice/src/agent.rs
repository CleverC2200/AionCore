use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum VoiceAgentError {
    #[error("conversation agent rejected the voice turn")]
    Rejected,
    #[error("conversation agent response stream was interrupted")]
    StreamInterrupted,
    #[error("conversation agent response timed out")]
    Timeout,
    #[error("conversation agent returned no speakable text")]
    EmptyResponse,
}

impl VoiceAgentError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::StreamInterrupted => "stream_interrupted",
            Self::Timeout => "timeout",
            Self::EmptyResponse => "empty_response",
        }
    }
}

#[async_trait]
pub trait VoiceConversationAgent: Send + Sync {
    async fn respond(&self, owner_user_id: &str, conversation_id: &str, text: &str) -> Result<String, VoiceAgentError>;
}
