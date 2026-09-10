mod common;

use aionui_db::{CreateProviderParams, IProviderRepository, SqliteProviderRepository};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{body_json, build_app, get_request, json_with_token, setup_and_login};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const URI: &str = "/api/models/inference";

async fn seed(services: &aionui_app::AppServices, base_url: &str, platform: &str, model: &str) {
    let key = aionui_app::derive_encryption_key(&services.jwt_secret_raw);
    let encrypted = aionui_common::encrypt_string("fake-provider-key", &key).unwrap();
    let repo = SqliteProviderRepository::new(services.database.pool().clone());
    repo.create(CreateProviderParams {
        id: Some("inference-provider"),
        user_id: "system_default_user",
        name: "Mock inference",
        platform,
        base_url,
        api_key_encrypted: &encrypted,
        models: &json!([model]).to_string(),
        enabled: true,
        capabilities: "[]",
        context_limit: None,
        model_protocols: None,
        model_enabled: None,
        model_health: None,
        model_settings: "{}",
        bedrock_config: None,
        is_full_url: false,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn model_inference_auth_csrf_and_invalid_input_are_rejected() {
    let (mut app, services) = build_app().await;
    let csrf_response = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let csrf = common::extract_csrf_token(&csrf_response).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri(URI)
        .header("content-type", "application/json")
        .header("x-csrf-token", &csrf)
        .header("cookie", format!("aionui-csrf-token={csrf}"))
        .body(Body::from(r#"{"question":"data"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let request = Request::builder()
        .method("POST")
        .uri(URI)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(r#"{"question":"data"}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
    for question in [String::new(), "x".repeat(65_537)] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                URI,
                json!({"question":question}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"], "MODEL_INFERENCE_INVALID_QUESTION");
    }
    for body in [
        json!({}),
        json!({"question": 7}),
        json!({"question":"data","conversation_id":"unrelated"}),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token("POST", URI, body, &token, &csrf))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"], "Invalid JSON request body.");
    }
}

#[tokio::test]
async fn model_inference_reuses_openai_and_anthropic_wire_adapters() {
    for (platform, model, endpoint, stream) in [
        (
            "openai",
            "gpt-4o",
            "/v1/chat/completions",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"sku\\\":\\\"check demand\\\"}\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ),
        (
            "anthropic",
            "claude-sonnet-4",
            "/v1/messages",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"sku\\\":\\\"check demand\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(stream),
            )
            .mount(&server)
            .await;
        let (mut app, services) = build_app().await;
        let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
        let base_url = if platform == "anthropic" {
            server.uri()
        } else {
            format!("{}/v1", server.uri())
        };
        seed(&services, &base_url, platform, model).await;
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                URI,
                json!({"question":"Analyze the supplied quantity 12"}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = body_json(response).await;
        assert_eq!(
            result["data"]["status"],
            "ok",
            "platform {platform}; paths={:?}",
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|request| request.url.path().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(result["data"]["model"], model);
        assert_eq!(result["data"]["answer"], "{\"sku\":\"check demand\"}");
        assert!(!result.to_string().contains("fake-provider-key"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], model);
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        assert_eq!(
            body["messages"].as_array().unwrap().len(),
            1 + usize::from(platform == "openai")
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations")
            .fetch_one(services.database.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
        let (other, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss1").await;
        let response = app
            .oneshot(json_with_token(
                "POST",
                URI,
                json!({"question":"data"}),
                &other,
                &other_csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"], "MODEL_NOT_SELECTED");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
