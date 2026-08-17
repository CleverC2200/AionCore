mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use aionui_api_types::{ManagedVoiceCapability, ManagedVoiceProvider, VoiceSessionRtcCredentials};
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use aionui_voice::{
    ManagedVoiceBackend, ProviderError, VoiceAgentError, VoiceConversationAgent, VoiceProviderSession,
    VoiceRouterState, VoiceService,
};

use common::{body_json, delete_with_token, get_request, get_with_token, json_with_token, setup_and_login};

struct MockVoiceBackend {
    starts: AtomicUsize,
    stops: AtomicUsize,
    interrupts: AtomicUsize,
    speaks: AtomicUsize,
}

struct MockVoiceAgent;

#[async_trait]
impl VoiceConversationAgent for MockVoiceAgent {
    async fn respond(
        &self,
        _owner_user_id: &str,
        _conversation_id: &str,
        _text: &str,
    ) -> Result<String, VoiceAgentError> {
        Ok("客户端 Agent 回复".to_owned())
    }
}

#[async_trait]
impl ManagedVoiceBackend for MockVoiceBackend {
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
        Ok(VoiceSessionRtcCredentials {
            app_id: "123456789012345678901234".to_owned(),
            room_id: session.room_id.clone(),
            user_id: session.user_id.clone(),
            token: "temporary-rtc-token".to_owned(),
        })
    }

    async fn start_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn stop_session(&self, _session: &VoiceProviderSession) -> Result<(), ProviderError> {
        self.stops.fetch_add(1, Ordering::Relaxed);
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

async fn build_app_with_voice_backend() -> (axum::Router, AppServices, Arc<MockVoiceBackend>) {
    let database = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(database, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await.expect("build module states");
    let backend = Arc::new(MockVoiceBackend {
        starts: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        interrupts: AtomicUsize::new(0),
        speaks: AtomicUsize::new(0),
    });
    states.voice = VoiceRouterState::new(VoiceService::new(backend.clone(), Arc::new(MockVoiceAgent)));
    let router = create_router_with_states(&services, states);
    (router, services, backend)
}

async fn build_app_with_voice_registry() -> (axum::Router, AppServices) {
    let database = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(database, &AppConfig::default()).await.unwrap();
    let (states, _) = build_module_states(&services).await.expect("build module states");
    let router = create_router_with_states(&services, states);
    (router, services)
}

fn voice_configuration_body(name: &str, enabled: bool) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "enabled": enabled,
        "rtc_app_id": "123456789012345678901234",
        "access_key": "access-key",
        "secret_key": "secret-key",
        "rtc_app_key": "rtc-app-key",
        "agent_user_id": "voice-agent",
        "welcome_message": "你好",
        "asr_app_id": "asr-app",
        "asr_cluster": "asr-cluster",
        "tts_app_id": "tts-app",
        "tts_cluster": "volcano-tts",
        "tts_voice_type": "BV001_streaming",
        "llm_url": "https://example.com",
        "llm_api_key": "llm-key",
        "llm_model_name": "voice-model",
        "system_message": "简洁回答"
    })
}

#[tokio::test]
async fn voice_routes_require_authentication_and_csrf() {
    let (mut app, services, _) = build_app_with_voice_backend().await;

    let response = app
        .clone()
        .oneshot(get_request("/api/voice/capabilities"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/voice/sessions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn voice_session_contract_is_owned_and_stop_is_idempotent() {
    let (mut app, services, backend) = build_app_with_voice_backend().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let response = app
        .clone()
        .oneshot(get_with_token("/api/voice/capabilities", &owner_token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["enabled"], true);
    assert_eq!(body["data"]["provider"], "volcengine-rtc");

    let request = json_with_token(
        "POST",
        "/api/voice/sessions",
        serde_json::json!({ "conversation_id": "conversation-1" }),
        &owner_token,
        &owner_csrf,
    );
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let session_id = body["data"]["session_id"].as_str().unwrap().to_owned();
    assert_eq!(body["data"]["rtc"]["app_id"], "123456789012345678901234");
    assert!(
        body["data"]["rtc"]["room_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        body["data"]["rtc"]["user_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(body["data"]["rtc"]["token"], "temporary-rtc-token");
    assert!(body["data"]["expires_at"].as_i64().is_some_and(|value| value > 0));

    let request = json_with_token(
        "POST",
        &format!("/api/voice/sessions/{session_id}/start"),
        serde_json::json!({}),
        &owner_token,
        &owner_csrf,
    );
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(backend.starts.load(Ordering::Relaxed), 1);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/voice/sessions/{session_id}/turns"),
            serde_json::json!({ "text": "查询当前客户" }),
            &owner_token,
            &owner_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["text"], "客户端 Agent 回复");
    assert_eq!(backend.interrupts.load(Ordering::Relaxed), 2);
    assert_eq!(backend.speaks.load(Ordering::Relaxed), 1);

    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other-user", "StrongP@ss2").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/voice/sessions/{session_id}/start"),
            serde_json::json!({}),
            &other_token,
            &other_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["code"], "VOICE_SESSION_NOT_FOUND");

    let response = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/voice/sessions/{session_id}"),
            &other_token,
            &other_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["code"], "VOICE_SESSION_NOT_FOUND");

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(delete_with_token(
                &format!("/api/voice/sessions/{session_id}"),
                &owner_token,
                &owner_csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(backend.stops.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn voice_configuration_crud_supports_multiple_redacted_entries() {
    let (mut app, services) = build_app_with_voice_registry().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/voice/configurations",
            voice_configuration_body("Production", true),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let first_id = body["data"]["id"].as_str().unwrap().to_owned();
    assert!(body["data"].get("access_key").is_none());
    assert_eq!(body["data"]["access_key_configured"], true);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/voice/configurations",
            voice_configuration_body("Backup", false),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let second_id = body["data"]["id"].as_str().unwrap().to_owned();

    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            &format!("/api/voice/configurations/{second_id}/enabled"),
            serde_json::json!({ "enabled": true }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(get_with_token("/api/voice/configurations", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == first_id)
            .unwrap()["enabled"],
        false
    );

    let response = app
        .oneshot(delete_with_token(
            &format!("/api/voice/configurations/{first_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
