use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::SendMessageRequest;
use aionui_conversation::ConversationService;
use aionui_realtime::BroadcastEventBus;
use aionui_voice::{VoiceAgentError, VoiceConversationAgent};
use async_trait::async_trait;
use tokio::sync::broadcast;

const VOICE_AGENT_TIMEOUT: Duration = Duration::from_secs(180);

fn apply_stream_text(response: &mut String, data: &serde_json::Value) {
    let Some(chunk) = data.get("data").and_then(|value| {
        value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .or(value.as_str())
    }) else {
        return;
    };
    if data.get("replace").and_then(serde_json::Value::as_bool) == Some(true) {
        response.clear();
    }
    response.push_str(chunk);
}

pub(crate) struct ConversationVoiceAgent {
    conversation_service: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
    event_bus: Arc<BroadcastEventBus>,
}

impl ConversationVoiceAgent {
    pub(crate) fn new(
        conversation_service: ConversationService,
        task_manager: Arc<dyn IWorkerTaskManager>,
        event_bus: Arc<BroadcastEventBus>,
    ) -> Self {
        Self {
            conversation_service,
            task_manager,
            event_bus,
        }
    }
}

#[async_trait]
impl VoiceConversationAgent for ConversationVoiceAgent {
    async fn respond(&self, owner_user_id: &str, conversation_id: &str, text: &str) -> Result<String, VoiceAgentError> {
        let mut events = self.event_bus.subscribe();
        let accepted = self
            .conversation_service
            .send_message(
                owner_user_id,
                conversation_id,
                SendMessageRequest {
                    content: text.to_owned(),
                    files: Vec::new(),
                    // Voice input has no `@@` conversation picker.
                    sessions: Vec::new(),
                    inject_skills: Vec::new(),
                    hidden: false,
                },
                &self.task_manager,
            )
            .await
            .map_err(|_| VoiceAgentError::Rejected)?;

        let turn_id = accepted.turn_id;
        let collect = async {
            let mut response = String::new();
            loop {
                let event = events.recv().await.map_err(|error| match error {
                    broadcast::error::RecvError::Closed | broadcast::error::RecvError::Lagged(_) => {
                        VoiceAgentError::StreamInterrupted
                    }
                })?;
                let data = &event.data;
                if data.get("user_id").and_then(serde_json::Value::as_str) != Some(owner_user_id)
                    || data.get("conversation_id").and_then(serde_json::Value::as_str) != Some(conversation_id)
                    || data.get("turn_id").and_then(serde_json::Value::as_str) != Some(turn_id.as_str())
                {
                    continue;
                }

                if event.name == "message.stream"
                    && matches!(
                        data.get("type").and_then(serde_json::Value::as_str),
                        Some("text" | "content")
                    )
                {
                    apply_stream_text(&mut response, data);
                }

                if event.name == "turn.completed" {
                    let response = response.trim().to_owned();
                    return if response.is_empty() {
                        Err(VoiceAgentError::EmptyResponse)
                    } else {
                        Ok(response)
                    };
                }
            }
        };

        tokio::time::timeout(VOICE_AGENT_TIMEOUT, collect)
            .await
            .map_err(|_| VoiceAgentError::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::apply_stream_text;

    #[test]
    fn final_middleware_override_replaces_streamed_text() {
        let mut response = String::new();
        apply_stream_text(&mut response, &json!({ "data": { "content": "draft" } }));
        apply_stream_text(
            &mut response,
            &json!({ "data": { "content": "final" }, "replace": true }),
        );
        assert_eq!(response, "final");
    }
}
