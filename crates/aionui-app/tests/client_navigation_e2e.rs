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
