//! Full-router V1 Client Navigation checks against a mocked GEA gateway.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as WiremockRequest, ResponseTemplate};

use common::{body_json, build_app_with_gea_base_url, json_with_token, setup_and_login};

#[tokio::test]
async fn current_user_resolves_v1_into_a_local_conversation_and_acknowledges_visibility() {
    let gea = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/client-navigation-intents/resolve"))
        .and(header("x-access-token", "gea-token"))
        .respond_with(|request: &WiremockRequest| {
            assert_eq!(
                request.body_json::<serde_json::Value>().expect("resolve request json"),
                json!({
                    "schemaVersion": 1,
                    "navigationReference": "reference.safe_123456"
                })
            );
            ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "navigationIntentId": "intent-1",
                    "schemaVersion": 1,
                    "target": {"type": "AGENT", "agentCode": "sales_forecast"},
                    "expiresAt": "2099-09-01T12:00:00Z",
                    "traceId": "trace-1"
                }
            }))
        })
        .expect(2)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/session"))
        .and(header("x-access-token", "gea-token"))
        .respond_with(|request: &WiremockRequest| {
            let body: serde_json::Value = request.body_json().expect("session request json");
            let conversation_id = body["conversationId"].as_str().expect("conversation id");
            assert_eq!(body["consumerCode"], "sales_forecast");
            ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": {"allowed": true},
                    "delegationToken": "delegation-token",
                    "gatewayContext": {
                        "consumerCode": "sales_forecast",
                        "sessionId": "session-1",
                        "conversationId": conversation_id
                    }
                }
            }))
        })
        .expect(2)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/client-navigation-intents/intent-1/ack"))
        .and(header("x-access-token", "gea-token"))
        .respond_with(|request: &WiremockRequest| {
            assert_eq!(
                request.body_json::<serde_json::Value>().expect("ack request json"),
                json!({
                    "stage": "TARGET_VISIBLE",
                    "result": "SUCCESS",
                    "idempotencyKey": "visible-ack-1"
                })
            );
            ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": null
            }))
        })
        .expect(1)
        .mount(&gea)
        .await;

    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let auth = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({"accessToken": "gea-token", "tenantId": "tenant-a"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(auth.status(), StatusCode::OK);

    let resolved = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/resolve",
            json!({
                "schema_version": 1,
                "navigation_reference": "reference.safe_123456"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let resolved_status = resolved.status();
    let resolved = body_json(resolved).await;
    assert_eq!(resolved_status, StatusCode::OK, "unexpected resolve body: {resolved}");
    assert_eq!(resolved["data"]["schema_version"], 1);
    assert_eq!(resolved["data"]["navigation_intent_id"], "intent-1");
    assert_eq!(resolved["data"]["target"]["type"], "conversation");
    let conversation_id = resolved["data"]["target"]["conversation_id"]
        .as_str()
        .expect("local conversation id")
        .to_owned();
    assert_eq!(resolved["data"]["trace_id"], "trace-1");
    for private_value in [
        "gea-token",
        "delegation-token",
        "reference.safe_123456",
        "tenant-a",
        "sales_forecast",
    ] {
        assert!(!resolved.to_string().contains(private_value));
    }

    let restored = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/resolve",
            json!({
                "schema_version": 1,
                "navigation_reference": "reference.safe_123456"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    assert_eq!(
        body_json(restored).await["data"]["target"]["conversation_id"],
        conversation_id
    );

    let acknowledged = app
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/ack",
            json!({
                "navigation_intent_id": "intent-1",
                "idempotency_key": "visible-ack-1"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(acknowledged.status(), StatusCode::OK);
}

#[tokio::test]
async fn client_navigation_rejects_unsupported_or_unknown_local_contract_fields_before_calling_gea() {
    let (mut app, services) = common::build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let unsupported = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/resolve",
            json!({"schema_version": 2, "navigation_reference": "reference.safe_123456"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_json(unsupported).await["code"], "NAVIGATION_SCHEMA_UNSUPPORTED");

    let unknown = app
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/resolve",
            json!({
                "schema_version": 1,
                "navigation_reference": "reference.safe_123456",
                "profile": "production"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(unknown).await["code"], "NAVIGATION_REQUEST_INVALID");
}

#[tokio::test]
async fn failed_gateway_session_rolls_back_a_new_navigation_conversation() {
    let gea = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/client-navigation-intents/resolve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "navigationIntentId": "intent-rollback",
                "schemaVersion": 1,
                "target": {"type": "AGENT", "agentCode": "sales_forecast"},
                "expiresAt": "2099-09-01T12:00:00Z",
                "traceId": "trace-rollback"
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "result": {
                "accessDecision": {"allowed": false, "reasonCode": "AGENT_ACCESS_DENIED"}
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;

    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let auth = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({"accessToken": "gea-token", "tenantId": "tenant-a"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(auth.status(), StatusCode::OK);

    let resolved = app
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/resolve",
            json!({
                "schema_version": 1,
                "navigation_reference": "reference.rollback_123456"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resolved.status(), StatusCode::FORBIDDEN);

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE extra LIKE '%client_navigation%'")
            .fetch_one(services.database.pool())
            .await
            .unwrap();
    assert_eq!(remaining, 0, "failed resolve must not leave an orphan conversation");
}

fn navigation_result() -> serde_json::Value {
    json!({
        "success": true,
        "result": {
            "navigationIntentId": "intent-1",
            "schemaVersion": 1,
            "target": {"type": "AGENT", "agentCode": "sales_forecast"},
            "expiresAt": "2099-09-01T12:00:00Z",
            "traceId": "trace-1"
        }
    })
}

#[tokio::test]
async fn navigation_accepts_gea_result_metadata_and_normalizes_configured_dates() {
    let gea = MockServer::start().await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    set_gea_identity(&app, &token, &csrf, "private-user-token", "tenant-a").await;

    // GEA Result<T> wraps ClientNavigationResolveResponse. Its Date field uses
    // the prod/test Jackson yyyy-MM-dd HH:mm:ss format and GMT+8 timezone.
    for (upstream_date, normalized_date) in [
        ("2099-09-01 12:00:00", Some("2099-09-01T12:00:00+08:00")),
        ("2099-09-01T04:00:00Z", Some("2099-09-01T04:00:00Z")),
        ("1999-09-01 12:00:00", None),
    ] {
        gea.reset().await;
        let mut result = navigation_result();
        result["result"]["expiresAt"] = json!(upstream_date);
        result.as_object_mut().unwrap().extend(
            json!({
                "message": "private-metadata-message",
                "code": 200,
                "timestamp": 1770000000000_i64,
                "errorCode": null,
                "category": null,
                "retryable": null,
                "requestId": "private-request-id",
                "traceId": "private-envelope-trace-id",
                "auditId": "private-audit-id",
                "details": {"diagnostic": "private-user-token reference.safe_123456"}
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        Mock::given(method("POST"))
            .and(path("/ai/gateway/client-navigation-intents/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(result))
            .expect(1)
            .mount(&gea)
            .await;
        mount_gateway_session(&gea).await;

        let (status, body) = resolve_navigation(&app, &token, &csrf).await;
        if let Some(normalized_date) = normalized_date {
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["data"]["expires_at"], normalized_date);
            assert_eq!(body["data"]["trace_id"], "trace-1");
            assert_eq!(body["data"].as_object().unwrap().len(), 5);
            assert_eq!(
                chrono::DateTime::parse_from_rfc3339(body["data"]["expires_at"].as_str().unwrap()).unwrap(),
                chrono::DateTime::parse_from_rfc3339("2099-09-01T04:00:00Z").unwrap(),
            );
        } else {
            assert_eq!(status, StatusCode::GONE, "{body}");
            assert_eq!(body["code"], "NAVIGATION_REFERENCE_EXPIRED");
            assert_eq!(gea.received_requests().await.unwrap().len(), 1);
        }
        assert!(!body.to_string().contains("private-"), "{body}");
        assert!(!body.to_string().contains("reference.safe_123456"), "{body}");
    }
}

async fn set_gea_identity(app: &axum::Router, token: &str, csrf: &str, access_token: &str, tenant: &str) {
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/gea/auth/session",
            json!({"accessToken": access_token, "tenantId": tenant}),
            token,
            csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

async fn resolve_navigation(app: &axum::Router, token: &str, csrf: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/resolve",
            json!({"schema_version": 1, "navigation_reference": "reference.safe_123456"}),
            token,
            csrf,
        ))
        .await
        .unwrap();
    (response.status(), body_json(response).await)
}

async fn mount_gateway_session(gea: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/ai/gateway/session"))
        .respond_with(|request: &WiremockRequest| {
            let body: serde_json::Value = request.body_json().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": {"allowed": true},
                    "delegationToken": "private-gateway-token",
                    "gatewayContext": {
                        "consumerCode": body["consumerCode"],
                        "sessionId": "session-1",
                        "conversationId": body["conversationId"]
                    }
                }
            }))
        })
        .mount(gea)
        .await;
}

#[tokio::test]
async fn malformed_upstream_navigation_is_rejected_before_creating_any_conversation() {
    let gea = MockServer::start().await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    set_gea_identity(&app, &token, &csrf, "private-user-token", "tenant-a").await;

    let mutations = [
        ("/unexpected", json!("private-field")),
        ("/result/unexpected", json!("private-field")),
        ("/result/target/unexpected", json!("private-field")),
        ("/result/schemaVersion", json!(2)),
        ("/result/target/type", json!("INTERACTION_REQUEST")),
        ("/result/navigationIntentId", json!("x".repeat(241))),
        ("/result/navigationIntentId", json!("..")),
        ("/result/navigationIntentId", json!("")),
        ("/result/target/agentCode", json!("x".repeat(241))),
        ("/result/target/agentCode", json!(42)),
        ("/result/traceId", json!("界".repeat(241))),
        ("/result/traceId", serde_json::Value::Null),
        ("/result/expiresAt", json!("invalid-date")),
    ];
    for (pointer, replacement) in mutations {
        gea.reset().await;
        let mut result = navigation_result();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        result
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), replacement);
        Mock::given(method("POST"))
            .and(path("/ai/gateway/client-navigation-intents/resolve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(result))
            .expect(1)
            .mount(&gea)
            .await;
        let (status, body) = resolve_navigation(&app, &token, &csrf).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{pointer}: {body}");
        assert_eq!(body["code"], "GEA_INVALID_RESPONSE", "{pointer}");
        assert!(!body.to_string().contains("private"));
        assert_eq!(gea.received_requests().await.unwrap().len(), 1);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE extra LIKE '%client_navigation%'")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn navigation_accepts_full_length_opaque_identifiers_and_encodes_ack_path() {
    let gea = MockServer::start().await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    set_gea_identity(&app, &token, &csrf, "private-user-token", "tenant-a").await;
    let mut result = navigation_result();
    result["result"]["navigationIntentId"] = json!("i".repeat(240));
    result["result"]["target"]["agentCode"] = json!("a".repeat(240));
    result["result"]["traceId"] = json!("界".repeat(240));
    Mock::given(method("POST"))
        .and(path("/ai/gateway/client-navigation-intents/resolve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(result))
        .expect(1)
        .mount(&gea)
        .await;
    mount_gateway_session(&gea).await;
    let (status, body) = resolve_navigation(&app, &token, &csrf).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["navigation_intent_id"], "i".repeat(240));
    assert_eq!(body["data"]["trace_id"], "界".repeat(240));
    assert!(!body.to_string().contains("private-"));

    Mock::given(method("POST"))
        .and(path(
            "/ai/gateway/client-navigation-intents/intent%2F..%2Fa%3Fx%23%25%26%E4%B8%AD/ack",
        ))
        .respond_with(|request: &WiremockRequest| {
            assert_eq!(request.url.query(), None);
            assert_eq!(
                request.body_json::<serde_json::Value>().unwrap(),
                json!({
                    "stage": "TARGET_VISIBLE", "result": "SUCCESS", "idempotencyKey": "键".repeat(256)
                })
            );
            ResponseTemplate::new(200).set_body_json(json!({"success": true, "result": null}))
        })
        .expect(1)
        .mount(&gea)
        .await;
    let acknowledged = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/deep-links/ack",
            json!({"navigation_intent_id": "intent/../a?x#%&中", "idempotency_key": "键".repeat(256)}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let status = acknowledged.status();
    let body = body_json(acknowledged).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    for intent in [".".to_owned(), "..".to_owned(), "i".repeat(241)] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/deep-links/ack",
                json!({"navigation_intent_id": intent, "idempotency_key": "key-1"}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["code"], "NAVIGATION_REQUEST_INVALID");
    }
}

#[tokio::test]
async fn navigation_reuses_conversations_only_for_the_same_user_tenant_and_gateway() {
    let gea = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/client-navigation-intents/resolve"))
        .respond_with(ResponseTemplate::new(200).set_body_json(navigation_result()))
        .expect(4)
        .mount(&gea)
        .await;
    mount_gateway_session(&gea).await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (alice_token, alice_csrf) = setup_and_login(&mut app, &services, "alice", "StrongP@ss1").await;
    let (bob_token, bob_csrf) = setup_and_login(&mut app, &services, "bob", "StrongP@ss1").await;
    set_gea_identity(&app, &alice_token, &alice_csrf, "private-alice", "tenant-a").await;
    let (status, alice) = resolve_navigation(&app, &alice_token, &alice_csrf).await;
    assert_eq!(status, StatusCode::OK, "{alice}");
    let (status, unauthenticated) = resolve_navigation(&app, &bob_token, &bob_csrf).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(unauthenticated["code"], "GEA_AUTH_REQUIRED");
    set_gea_identity(&app, &bob_token, &bob_csrf, "private-bob", "tenant-a").await;
    let (status, bob) = resolve_navigation(&app, &bob_token, &bob_csrf).await;
    assert_eq!(status, StatusCode::OK, "{bob}");
    set_gea_identity(&app, &alice_token, &alice_csrf, "private-alice-b", "tenant-b").await;
    let (status, other_tenant) = resolve_navigation(&app, &alice_token, &alice_csrf).await;
    assert_eq!(status, StatusCode::OK, "{other_tenant}");
    set_gea_identity(&app, &alice_token, &alice_csrf, "private-alice-refreshed", "tenant-a").await;
    let (status, restored) = resolve_navigation(&app, &alice_token, &alice_csrf).await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    let id = |body: &serde_json::Value| body["data"]["target"]["conversation_id"].as_str().unwrap().to_owned();
    assert_ne!(id(&alice), id(&bob));
    assert_ne!(id(&alice), id(&other_tenant));
    assert_eq!(id(&alice), id(&restored));
    let extras: Vec<String> =
        sqlx::query_scalar("SELECT extra FROM conversations WHERE extra LIKE '%client_navigation%'")
            .fetch_all(services.database.pool())
            .await
            .unwrap();
    assert_eq!(extras.len(), 3);
    assert!(
        extras
            .iter()
            .all(|extra| !extra.contains("private-") && !extra.contains("reference.safe"))
    );
}

#[tokio::test]
async fn identity_change_during_resolve_discards_the_stale_result_without_creating_a_conversation() {
    let gea = MockServer::start().await;
    let resolving = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = resolving.clone();
    Mock::given(method("POST"))
        .and(path("/ai/gateway/client-navigation-intents/resolve"))
        .respond_with(move |_: &WiremockRequest| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(navigation_result())
                .set_delay(std::time::Duration::from_millis(250))
        })
        .expect(1)
        .mount(&gea)
        .await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    set_gea_identity(&app, &token, &csrf, "private-old", "tenant-a").await;
    let pending_app = app.clone();
    let pending_token = token.clone();
    let pending_csrf = csrf.clone();
    let pending = tokio::spawn(async move { resolve_navigation(&pending_app, &pending_token, &pending_csrf).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), resolving.notified())
        .await
        .unwrap();
    set_gea_identity(&app, &token, &csrf, "private-new", "tenant-b").await;
    let (status, body) = pending.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "NAVIGATION_IDENTITY_CHANGED");
    assert_eq!(gea.received_requests().await.unwrap().len(), 1);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE extra LIKE '%client_navigation%'")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn navigation_upstream_failures_never_disclose_credentials_references_or_diagnostic_details() {
    let gea = MockServer::start().await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    set_gea_identity(&app, &token, &csrf, "private-user-token", "tenant-a").await;
    for endpoint in [
        "/ai/gateway/client-navigation-intents/resolve",
        "/ai/gateway/session",
        "/ai/gateway/client-navigation-intents/intent-1/ack",
    ] {
        gea.reset().await;
        if endpoint == "/ai/gateway/session" {
            Mock::given(method("POST"))
                .and(path("/ai/gateway/client-navigation-intents/resolve"))
                .respond_with(ResponseTemplate::new(200).set_body_json(navigation_result()))
                .mount(&gea)
                .await;
        }
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false, "errorCode": "NAVIGATION_REFERENCE_FORBIDDEN",
                "message": "private-user-token reference.safe_123456",
                "traceId": "private-trace", "requestId": "private-request", "auditId": "private-audit",
                "details": {"delegationToken": "private-gateway-token"}
            })))
            .expect(1)
            .mount(&gea)
            .await;
        let (status, body) = if endpoint.ends_with("/ack") {
            let response = app
                .clone()
                .oneshot(json_with_token(
                    "POST",
                    "/api/deep-links/ack",
                    json!({"navigation_intent_id": "intent-1", "idempotency_key": "key-1"}),
                    &token,
                    &csrf,
                ))
                .await
                .unwrap();
            (response.status(), body_json(response).await)
        } else {
            resolve_navigation(&app, &token, &csrf).await
        };
        assert_eq!(status, StatusCode::FORBIDDEN, "{endpoint}: {body}");
        assert_eq!(body["code"], "NAVIGATION_REFERENCE_FORBIDDEN");
        assert!(!body.to_string().contains("private-"), "{body}");
        assert!(!body.to_string().contains("reference.safe_123456"), "{body}");
    }
}

#[tokio::test]
async fn navigation_routes_require_core_authentication_and_csrf_before_calling_gea() {
    let gea = MockServer::start().await;
    let (mut app, services) = build_app_with_gea_base_url(gea.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    for (endpoint, payload) in [
        (
            "/api/deep-links/resolve",
            json!({"schema_version": 1, "navigation_reference": "reference.safe_123456"}),
        ),
        (
            "/api/deep-links/ack",
            json!({"navigation_intent_id": "intent-1", "idempotency_key": "key-1"}),
        ),
    ] {
        let mut unauthenticated = json_with_token("POST", endpoint, payload.clone(), &token, &csrf);
        unauthenticated.headers_mut().remove("authorization");
        let response = app.clone().oneshot(unauthenticated).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let mut missing_csrf = json_with_token("POST", endpoint, payload, &token, &csrf);
        missing_csrf.headers_mut().remove("x-csrf-token");
        let response = app.clone().oneshot(missing_csrf).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
    }
    assert!(gea.received_requests().await.unwrap().is_empty());
}
