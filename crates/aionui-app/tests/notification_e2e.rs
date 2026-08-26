//! Full-router contract checks for the tenant-scoped GEA Notification API.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{
    body_json as response_json, build_app, build_app_with_gea_base_url, get_request, get_with_token, json_with_token,
    setup_and_login,
};

#[derive(Clone)]
struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SharedLogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn snapshot() -> serde_json::Value {
    json!({
        "success": true,
        "result": {
            "items": [{
                "id": "notification-1",
                "version": 1,
                "state": "unread",
                "kind": "workflow_event",
                "severity": "warning",
                "title": "Forecast needs review",
                "summary": "September forecast",
                "dismissible": true,
                "source": {"type": "business_system", "ref": "forecast-1", "label": "gea.workflow"},
                "target": {"type": "aggregate", "value": "forecast-1"},
                "created_at": "2026-08-22T08:00:00Z"
            }, {
                "id": "notification-expired",
                "version": 1,
                "state": "unread",
                "kind": "reminder",
                "severity": "info",
                "title": "Expired reminder",
                "dismissible": true,
                "source": {"type": "business_system", "ref": "reminder-1", "label": "gea.workflow"},
                "target": {"type": "aggregate", "value": "reminder-1"},
                "created_at": "2020-01-01T00:00:00Z",
                "expires_at": "2020-01-02T00:00:00Z"
            }],
            "unread_count": 2,
            "total": 2
        }
    })
}

async fn seed_interaction_request(services: &aionui_app::AppServices, request_id: &str) {
    let conversation_id = format!("conversation-{request_id}");
    sqlx::query(
        "INSERT INTO conversations \
            (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
         VALUES (?, 'system_default_user', 'GEA fixture', 'aionrs', '{}', 'running', 0, 1, 1)",
    )
    .bind(&conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gea_interaction_requests \
            (user_id, request_id, conversation_id, version, status, kind, title, allowed_actions, \
             updated_at, presentation, upstream_revision, message_id, changed_at) \
         VALUES ('system_default_user', ?, ?, 'v1', 'pending', 'question', 'Choose', '[\"answer\"]', \
                 '2026-08-22T08:00:00Z', '{\"type\":\"question\",\"questions\":[]}', 'r1', \
                 'message-linked', 1)",
    )
    .bind(request_id)
    .bind(conversation_id)
    .execute(services.database.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn notification_routes_require_aioncore_authentication() {
    let (app, _) = build_app().await;
    let response = app
        .oneshot(get_request("/api/notifications?status=active"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn notification_sync_detail_action_replay_and_last_good_form_a_closed_loop() {
    let gea = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(snapshot()))
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/notifications/notification-1/read"))
        .and(body_json(json!({
            "expectedVersion": "1",
            "idempotencyKey": "command-read-1"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "receipt_id": "receipt-read-1",
                "notification_id": "notification-1",
                "version": 2,
                "status": "read"
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;

    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=active", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(
        body["data"]["revision"]
            .as_str()
            .is_some_and(|revision| revision.starts_with("gea-sha256:")),
        "unexpected notification list response: {body}"
    );
    assert_eq!(body["data"]["sync_state"], "fresh");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"]["items"][0]["id"], "notification-1");
    assert_eq!(body["data"]["items"][0]["target"]["type"], "notification");
    assert_eq!(body["data"]["items"][0]["kind"], "event");

    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications/notification-1", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["data"]["summary"], "September forecast");

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/notifications/notification-1/read",
                json!({
                    "expected_version": "1",
                    "idempotency_key": "command-read-1"
                }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["data"]["receipt_id"], "receipt-read-1");
        assert_eq!(body["data"]["status"], "read");
    }

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-1/dismiss",
            json!({
                "expected_version": "1",
                "idempotency_key": "command-read-1"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    gea.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "2"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .with_priority(1)
        .mount(&gea)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "items": [{
                    "id": "notification-partial",
                    "version": 1,
                    "state": "unread",
                    "kind": "event",
                    "severity": "info",
                    "title": "Partial page",
                    "dismissible": true,
                    "source": {"type": "business_system", "ref": "partial-1"},
                    "target": {"type": "aggregate", "value": "partial-1"},
                    "created_at": "2026-08-22T08:00:00Z"
                }],
                "unread_count": 2,
                "total": 2
            }
        })))
        .expect(1)
        .with_priority(10)
        .mount(&gea)
        .await;
    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=all", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["sync_state"], "partial");
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["items"][0]["status"], "read");
    assert!(!body["data"]["failure_codes"].as_array().unwrap().is_empty());

    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-b" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications/notification-1", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    gea.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": { "items": [], "unread_count": 0, "total": 0 }
        })))
        .mount(&gea)
        .await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .oneshot(get_with_token("/api/notifications?status=active", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["data"]["sync_state"], "fresh");
    assert!(body["data"]["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn notification_action_rejects_missing_csrf_before_dispatch() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/notifications/notification-1/read")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::from(
            json!({ "expected_version": "v1", "idempotency_key": "command-1" }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(response_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn concurrent_list_requests_share_one_upstream_snapshot_sync() {
    let gea = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(snapshot()),
        )
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let request_a = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=active", &token));
    let request_b = app.oneshot(get_with_token("/api/notifications?status=active", &token));
    let (response_a, response_b) = tokio::join!(request_a, request_b);
    let mut states = Vec::new();
    for response in [response_a.unwrap(), response_b.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        states.push(body["data"]["sync_state"].as_str().unwrap().to_owned());
        if body["data"]["sync_state"] == "fresh" {
            assert!(
                body["data"]["revision"]
                    .as_str()
                    .is_some_and(|revision| revision.starts_with("gea-sha256:"))
            );
        }
    }
    states.sort();
    assert_eq!(states, ["fresh", "syncing"]);
}

#[tokio::test]
async fn notification_action_conflicts_return_the_current_safe_state_matrix() {
    let gea = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(snapshot()))
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=active", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let version_conflict = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-1/read",
            json!({ "expected_version": "stale-v0", "idempotency_key": "version-conflict" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(version_conflict.status(), StatusCode::CONFLICT);
    let body = response_json(version_conflict).await;
    assert_eq!(body["code"], "GEA_NOTIFICATION_VERSION_CONFLICT");
    assert_eq!(body["details"]["upstream"]["version"], "1");
    assert_eq!(body["details"]["upstream"]["status"], "unread");

    let expired = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-expired/read",
            json!({ "expected_version": "1", "idempotency_key": "expired-read" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(expired.status(), StatusCode::CONFLICT);
    assert_eq!(response_json(expired).await["code"], "GEA_NOTIFICATION_EXPIRED");

    sqlx::query(
        "UPDATE gea_notifications SET version = 'v2', status = 'read' \
         WHERE user_id = 'system_default_user' AND tenant_id = 'tenant-a' AND notification_id = 'notification-1'",
    )
    .execute(services.database.pool())
    .await
    .unwrap();
    let already_read = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-1/read",
            json!({ "expected_version": "v2", "idempotency_key": "already-read" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(already_read.status(), StatusCode::CONFLICT);
    assert_eq!(
        response_json(already_read).await["code"],
        "GEA_NOTIFICATION_ALREADY_READ"
    );

    sqlx::query(
        "UPDATE gea_notifications SET version = 'v3', status = 'dismissed' \
         WHERE user_id = 'system_default_user' AND tenant_id = 'tenant-a' AND notification_id = 'notification-1'",
    )
    .execute(services.database.pool())
    .await
    .unwrap();
    let already_dismissed = app
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-1/read",
            json!({ "expected_version": "v3", "idempotency_key": "already-dismissed" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(already_dismissed.status(), StatusCode::CONFLICT);
    assert_eq!(
        response_json(already_dismissed).await["code"],
        "GEA_NOTIFICATION_ALREADY_DISMISSED"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn notification_action_logs_are_terminal_and_exclude_sensitive_content() {
    let gea = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(snapshot()))
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test-sensitive", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = SharedLogBuffer(Arc::clone(&bytes));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=active", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-1/read",
            json!({
                "expected_version": "stale-v0",
                "idempotency_key": "secret-idempotency-key"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    drop(_guard);

    let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("notification.action.started"));
    assert!(logs.contains("notification.action.conflicted"));
    assert!(logs.contains("notification.sync.started"));
    assert!(logs.contains("notification.sync.succeeded"));
    assert!(logs.contains("notification.projection.reconciled"));
    assert!(logs.contains("revision_before"));
    assert!(logs.contains("revision_after"));
    assert!(logs.contains("idempotency_key_hash"));
    for secret in [
        "secret-idempotency-key",
        "access-test-sensitive",
        "Forecast needs review",
        "September forecast",
        "Review the changed forecast before submission.",
    ] {
        assert!(!logs.contains(secret), "logs must not contain {secret}");
    }
}

#[tokio::test]
async fn snapshot_reconciliation_cannot_interrupt_an_in_flight_action_receipt() {
    let gea = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(snapshot()))
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=active", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    gea.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/notifications/notification-1/read"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_json(json!({
                    "success": true,
                    "result": {
                        "receipt_id": "receipt-concurrent",
                        "notification_id": "notification-1",
                        "version": 2,
                        "status": "read"
                    }
                })),
        )
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": { "items": [], "unread_count": 0, "total": 0 }
        })))
        .expect(1)
        .mount(&gea)
        .await;

    let action_app = app.clone();
    let action_token = token.clone();
    let action_csrf = csrf.clone();
    let action = tokio::spawn(async move {
        action_app
            .oneshot(json_with_token(
                "POST",
                "/api/notifications/notification-1/read",
                json!({ "expected_version": "1", "idempotency_key": "concurrent-read" }),
                &action_token,
                &action_csrf,
            ))
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if gea
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.method.as_str() == "POST")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the action request should reach GEA");

    let list = app.oneshot(get_with_token("/api/notifications?status=active", &token));
    let (action_response, list_response) = tokio::join!(action, list);
    assert_eq!(action_response.unwrap().status(), StatusCode::OK);
    assert_eq!(list_response.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn dismissibility_and_interaction_request_state_remain_independent() {
    let gea = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/notifications"))
        .and(query_param("pageNo", "1"))
        .and(query_param("pageSize", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "items": [{
                    "id": "notification-linked",
                    "version": 1,
                    "state": "unread",
                    "kind": "action_required",
                    "severity": "warning",
                    "title": "Review required",
                    "dismissible": false,
                    "source": {"type": "business_system", "ref": "request-linked", "label": "gea.workflow"},
                    "interaction_request_id": "request-linked",
                    "target": { "type": "aggregate", "value": "request-linked" },
                    "created_at": "2026-08-22T08:00:00Z"
                }],
                "unread_count": 1,
                "total": 1
            }
        })))
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/notifications/notification-linked/read"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "receipt_id": "receipt-linked",
                "notification_id": "notification-linked",
                "version": 2,
                "status": "read"
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    seed_interaction_request(&services, "request-linked").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({ "accessToken": "access-test", "tenantId": "tenant-a" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/notifications?status=active", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-linked/dismiss",
            json!({ "expected_version": "1", "idempotency_key": "dismiss-linked" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/notifications/notification-linked/read",
            json!({ "expected_version": "1", "idempotency_key": "read-linked" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let interaction_status: String = sqlx::query_scalar(
        "SELECT status FROM gea_interaction_requests \
         WHERE user_id = 'system_default_user' AND request_id = 'request-linked'",
    )
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert_eq!(interaction_status, "pending");

    sqlx::query(
        "UPDATE gea_interaction_requests SET status = 'resolved' \
         WHERE user_id = 'system_default_user' AND request_id = 'request-linked'",
    )
    .execute(services.database.pool())
    .await
    .unwrap();
    let notification_status: String = sqlx::query_scalar(
        "SELECT status FROM gea_notifications \
         WHERE user_id = 'system_default_user' AND tenant_id = 'tenant-a' \
           AND notification_id = 'notification-linked'",
    )
    .fetch_one(services.database.pool())
    .await
    .unwrap();
    assert_eq!(notification_status, "read");
}
