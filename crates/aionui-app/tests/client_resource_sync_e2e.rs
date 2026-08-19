//! Full-router GEA managed Skill synchronization checks.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request as WiremockRequest, ResponseTemplate};

use common::{body_json, build_app, build_app_with_gea_base_url, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn sync_requires_authentication() {
    let (app, _) = build_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/client-resources/sync")
                .header("content-type", "application/json")
                .header("cookie", "aionui-csrf-token=unauthenticated")
                .header("x-csrf-token", "unauthenticated")
                .body(Body::from(r#"{"resources":["skills"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sync_rejects_missing_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/client-resources/sync")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"resources":["skills"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn sync_rejects_an_empty_resource_list() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/client-resources/sync",
            json!({"resources": []}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "GEA_INVALID_REQUEST");
}

#[tokio::test]
async fn mcp_test_requires_authentication() {
    let (app, _) = build_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/gea/mcp/test")
                .header("content-type", "application/json")
                .header("cookie", "aionui-csrf-token=unauthenticated")
                .header("x-csrf-token", "unauthenticated")
                .body(Body::from(r#"{"consumerCode":"sales_forecast"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mcp_test_rejects_missing_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/gea/mcp/test")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"consumerCode":"sales_forecast"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn mcp_test_rejects_invalid_input_with_standard_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .oneshot(json_with_token("POST", "/api/gea/mcp/test", json!({}), &token, &csrf))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "GEA_INVALID_REQUEST");
}

#[tokio::test]
async fn sync_downloads_skill_and_exposes_it_to_the_authenticated_user() {
    let gea = MockServer::start().await;
    let skill_body = b"---\nname: sales-forecast\ndescription: Query forecasts\n---\nUse the governed forecast source.";
    let digest = format!("{:x}", Sha256::digest(skill_body));
    Mock::given(method("GET"))
        .and(path("/aidata/client-resource-catalog/my"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "ok",
            "revision": "resource-r1",
            "snapshot": {
                "schemaVersion": 1,
                "revision": "resource-r1",
                "tenantId": "tenant-a",
                "skills": [{
                    "id": "sales-forecast",
                    "version": "1.0.0",
                    "name": "Sales forecast",
                    "description": "Query forecasts",
                    "artifactRef": "skills/sales-forecast/1.0.0",
                    "digest": digest,
                    "artifactSize": skill_body.len(),
                    "state": "active"
                }]
            }
        })))
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/session"))
        .respond_with(|request: &WiremockRequest| {
            let body: serde_json::Value = request.body_json().unwrap();
            let conversation_id = body["conversationId"].as_str().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "probe-delegation",
                    "gatewayContext": {
                        "consumerCode": "sales_forecast",
                        "sessionId": "probe-session",
                        "conversationId": conversation_id
                    }
                }
            }))
        })
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("POST"))
        .and(path("/ai/gateway/mcp/proxy/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "tools": [{
                "name": "query_business_data",
                "sourceCode": "cube",
                "description": "Query business data",
                "inputSchema": { "type": "object" }
            }]
        })))
        .expect(1)
        .mount(&gea)
        .await;
    Mock::given(method("GET"))
        .and(path("/aidata/client-resource-catalog/skill-artifact"))
        .and(query_param("skillCode", "sales-forecast"))
        .and(query_param("version", "1.0.0"))
        .and(query_param("format", "md"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-skill-digest", digest.as_str())
                .insert_header("x-skill-size", skill_body.len().to_string().as_str())
                .insert_header("x-skill-version", "1.0.0")
                .set_body_bytes(skill_body),
        )
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

    let sync = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/client-resources/sync",
            json!({"resources": ["skills"]}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(sync.status(), StatusCode::OK);
    let sync_body = body_json(sync).await;
    assert_eq!(sync_body["data"]["status"], "completed");
    assert_eq!(sync_body["data"]["changed"], 1);
    assert!(
        !serde_json::to_string(&sync_body).unwrap().contains("gea-token"),
        "GEA credentials must never be echoed by the sync endpoint"
    );

    let listed = app
        .clone()
        .oneshot(get_with_token("/api/skills", &token))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = body_json(listed).await;
    let skill = listed_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["skill_id"] == "sales-forecast")
        .expect("managed Skill should be listed");
    assert_eq!(skill["source"], "managed");
    assert_eq!(skill["version"], "1.0.0");
    assert_eq!(skill["state"], "active");
    assert!(!serde_json::to_string(&listed_body).unwrap().contains("gea-token"));

    let mcp_test = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/gea/mcp/test",
            json!({"consumerCode": "sales_forecast"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(mcp_test.status(), StatusCode::OK);
    let mcp_body = body_json(mcp_test).await;
    assert_eq!(mcp_body["data"][0]["name"], "query_business_data");
    assert!(!serde_json::to_string(&mcp_body).unwrap().contains("probe-delegation"));

    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let other_listed = app
        .clone()
        .oneshot(get_with_token("/api/skills", &other_token))
        .await
        .unwrap();
    assert_eq!(other_listed.status(), StatusCode::OK);
    assert!(
        body_json(other_listed).await["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["skill_id"] != "sales-forecast"),
        "managed Skills must stay scoped to the authenticated Core user"
    );

    let other_mcp_test = app
        .oneshot(json_with_token(
            "POST",
            "/api/gea/mcp/test",
            json!({"consumerCode": "sales_forecast"}),
            &other_token,
            &other_csrf,
        ))
        .await
        .unwrap();
    assert_eq!(other_mcp_test.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(other_mcp_test).await["code"], "GEA_AUTH_REQUIRED");
}
