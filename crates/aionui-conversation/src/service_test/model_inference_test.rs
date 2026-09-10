use super::*;
use aionui_ai_agent::ModelInferencePort;
use aionui_api_types::{ModelInferenceRequest, ModelInferenceResponse, ModelInferenceStatus};
use aionui_common::ProviderWithModel;

struct RecordingInference {
    calls: Mutex<Vec<(String, ProviderWithModel)>>,
    change_selection: Option<Arc<MockRepo>>,
}

#[async_trait::async_trait]
impl ModelInferencePort for RecordingInference {
    async fn infer(
        &self,
        user_id: &str,
        model: ProviderWithModel,
        _question: String,
    ) -> Result<ModelInferenceResponse, AgentError> {
        self.calls.lock().unwrap().push((user_id.to_owned(), model.clone()));
        if let Some(repo) = &self.change_selection {
            repo.rows.lock().unwrap()[0].model = Some(r#"{"provider_id":"p2","model":"new"}"#.into());
        }
        Ok(ModelInferenceResponse {
            status: ModelInferenceStatus::Ok,
            provider_id: model.provider_id,
            model: model.use_model.unwrap_or(model.model),
            answer: Some("{\"sku\":\"advice\"}".into()),
        })
    }
}

#[tokio::test]
async fn model_inference_uses_current_selection_and_never_creates_messages_or_tasks() {
    let (service, _, repo, manager) = make_service();
    let row = insert_conversation_with_type(&repo, "owner", AgentType::Aionrs).await;
    repo.rows.lock().unwrap()[0].model =
        Some(r#"{"provider_id":"provider","model":"old","use_model":"current"}"#.into());
    let port = Arc::new(RecordingInference {
        calls: Mutex::new(vec![]),
        change_selection: None,
    });
    service.with_model_inference(port.clone());
    let result = service
        .infer_model(
            "owner",
            &row.id,
            ModelInferenceRequest {
                question: "data".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.model, "current");
    assert_eq!(port.calls.lock().unwrap()[0].0, "owner");
    assert_eq!(port.calls.lock().unwrap()[0].1.provider_id, "provider");
    assert!(repo.messages.lock().unwrap().is_empty());
    assert!(manager.get_task(&row.id).is_none());
}

#[tokio::test]
async fn model_inference_rejects_foreign_conversations_before_calling_provider() {
    let (service, _, repo, _) = make_service();
    let row = insert_conversation_with_type(&repo, "owner", AgentType::Aionrs).await;
    let port = Arc::new(RecordingInference {
        calls: Mutex::new(vec![]),
        change_selection: None,
    });
    service.with_model_inference(port.clone());
    let error = service
        .infer_model(
            "other",
            &row.id,
            ModelInferenceRequest {
                question: "data".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ConversationError::NotFound { .. }));
    assert!(port.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn model_inference_drops_a_response_after_selection_changes() {
    let (service, _, repo, _) = make_service();
    let row = insert_conversation_with_type(&repo, "owner", AgentType::Aionrs).await;
    repo.rows.lock().unwrap()[0].model = Some(r#"{"provider_id":"p1","model":"old"}"#.into());
    service.with_model_inference(Arc::new(RecordingInference {
        calls: Mutex::new(vec![]),
        change_selection: Some(repo),
    }));
    let error = service
        .infer_model(
            "owner",
            &row.id,
            ModelInferenceRequest {
                question: "data".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ConversationError::Busy { reason } if reason == "MODEL_SELECTION_CHANGED"));
}
