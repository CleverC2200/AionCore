//! Full-router security and ownership checks for the global Interaction Request API.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{
    body_json, build_app, build_app_with_gea_base_url, get_request, get_with_token, json_with_token, setup_and_login,
};

async fn seed_request(services: &aionui_app::AppServices, user_id: &str, request_id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES (?, ?, 'not-used', 1, 1)",
    )
    .bind(user_id)
    .bind(user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    let conversation_id = format!("conversation-{request_id}");
    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
         VALUES (?, ?, 'GEA fixture', 'aionrs', '{}', 'running', 0, 1, 1)",
    )
    .bind(&conversation_id)
    .bind(user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gea_interaction_requests \
            (user_id, request_id, conversation_id, version, status, kind, title, allowed_actions, \
             updated_at, presentation, upstream_revision, turn_id, message_id, changed_at) \
         VALUES (?, ?, ?, 'v1', 'pending', 'question', 'Choose a cost center', '[\"answer\"]', \
                 '2026-08-17T10:00:00+08:00', '{\"type\":\"question\",\"questions\":[]}', \
                 'r1', 'turn-1', ?, 1)",
    )
    .bind(user_id)
    .bind(request_id)
    .bind(conversation_id)
    .bind(format!("message-{request_id}"))
    .execute(services.database.pool())
    .await
    .unwrap();
}

async fn seed_finalized_receipt(
    services: &aionui_app::AppServices,
    user_id: &str,
    request_id: &str,
    idempotency_key: &str,
) {
    sqlx::query(
        "INSERT INTO gea_interaction_request_receipts \
            (user_id, request_id, idempotency_key, expected_version, action_id, receipt, created_at, finalized_at) \
         VALUES (?, ?, ?, 'v1', 'answer', ?, 1, 1)",
    )
    .bind(user_id)
    .bind(request_id)
    .bind(idempotency_key)
    .bind(
        json!({
            "receipt_id": "receipt-replay",
            "request_id": request_id,
            "version": "v2",
            "status": "accepted"
        })
        .to_string(),
    )
    .execute(services.database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn global_list_requires_authentication() {
    let (app, _services) = build_app().await;
    let response = app
        .oneshot(get_request("/api/interaction-requests?status=pending"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn global_list_returns_only_the_authenticated_users_projection() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    seed_request(&services, "system_default_user", "request-owned").await;
    seed_request(&services, "foreign-user", "request-foreign").await;

    let response = app
        .oneshot(get_with_token("/api/interaction-requests?status=pending", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "request-owned");
    assert_eq!(items[0]["turn_id"], "turn-1");
}

#[tokio::test]
async fn global_action_rejects_missing_csrf_before_dispatch() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/interaction-requests/request-owned/actions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            json!({
                "expected_version": "v1",
                "idempotency_key": "command-1",
                "action_id": "answer"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn global_action_requires_authentication() {
    let (app, _services) = build_app().await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/interaction-requests/request-owned/actions")
        .header("content-type", "application/json")
        .header("cookie", "aionui-csrf-token=unauthenticated-test")
        .header("x-csrf-token", "unauthenticated-test")
        .body(Body::from(
            json!({
                "expected_version": "v1",
                "idempotency_key": "command-unauthenticated",
                "action_id": "answer"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn global_action_rejects_invalid_input_with_the_standard_error_shape() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/interaction-requests/request-owned/actions",
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], "GEA_INVALID_REQUEST");
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn global_action_replays_a_finalized_success_receipt_through_the_full_route() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    seed_request(&services, "system_default_user", "request-replay").await;
    seed_finalized_receipt(&services, "system_default_user", "request-replay", "command-replay").await;

    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/interaction-requests/request-replay/actions",
            json!({
                "expected_version": "v1",
                "idempotency_key": "command-replay",
                "action_id": "answer"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["receipt_id"], "receipt-replay");
    assert_eq!(body["data"]["status"], "accepted");
}

#[tokio::test]
async fn global_action_returns_a_stable_business_conflict_receipt() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    seed_request(&services, "system_default_user", "request-conflict").await;
    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/interaction-requests/request-conflict/actions",
            json!({
                "expected_version": "stale-version",
                "idempotency_key": "command-conflict",
                "action_id": "answer"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["status"], "conflict");
    assert_eq!(body["data"]["request"]["status"], "pending");
}

#[tokio::test]
async fn global_action_completes_the_first_full_router_write_against_gea() {
    let gea = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "accessDecision": { "allowed": true },
                "delegationToken": "delegation-test",
                "gatewayContext": {
                    "consumerCode": "agent-sales",
                    "sessionId": "gea-session-route",
                    "conversationId": "conversation-request-first-write"
                }
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("GET"))
        .and(path("/ai/gateway/interaction-requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "revision": "route-r1",
                "items": [{
                    "id": "request-first-write",
                    "version": "v1",
                    "status": "pending",
                    "kind": "question",
                    "title": "Choose a cost center",
                    "allowedActions": ["answer", "decline"],
                    "updatedAt": "2026-08-17T10:00:10+08:00",
                    "presentation": {
                        "type": "question",
                        "questions": [{
                            "question": "Which cost center?",
                            "multiSelect": false,
                            "options": [{ "label": "CC-100" }]
                        }]
                    }
                }]
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/interaction-requests/request-first-write/actions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "receiptId": "receipt-first-write",
                "requestId": "request-first-write",
                "version": "v2",
                "status": "accepted",
                "turnContinuation": "original_tool_call_released"
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    seed_request(&services, "system_default_user", "request-first-write").await;
    sqlx::query("DELETE FROM gea_interaction_requests WHERE request_id = 'request-first-write'")
        .execute(services.database.pool())
        .await
        .unwrap();

    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-test" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/gea/conversations/conversation-request-first-write/session",
            json!({ "consumerCode": "agent-sales" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(get_with_token(
            "/api/gea/conversations/conversation-request-first-write/interaction-requests",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    sqlx::query("UPDATE gea_interaction_requests SET turn_id = 'turn-first-write' WHERE request_id = ?")
        .bind("request-first-write")
        .execute(services.database.pool())
        .await
        .unwrap();

    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/interaction-requests/request-first-write/actions",
            json!({
                "expected_version": "v1",
                "idempotency_key": "command-first-write",
                "action_id": "answer",
                "payload": { "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["receipt_id"], "receipt-first-write");
    assert_eq!(body["data"]["status"], "accepted");
    let status: String = sqlx::query_scalar(
        "SELECT status FROM gea_interaction_requests WHERE user_id = 'system_default_user' AND request_id = ?",
    )
    .bind("request-first-write")
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert_eq!(status, "resolved");
}

#[tokio::test]
async fn global_action_cannot_address_another_users_request() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    seed_request(&services, "foreign-user", "request-foreign").await;
    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/interaction-requests/request-foreign/actions",
            json!({
                "expected_version": "v1",
                "idempotency_key": "command-foreign",
                "action_id": "answer"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
