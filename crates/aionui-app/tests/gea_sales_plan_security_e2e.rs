mod common;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use wiremock::matchers::{
    body_json as matches_body_json, body_string_contains, header as matches_header, method, path,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{body_json, build_app, get_request, json_with_token, setup_and_login};

const HOST_SECRET: &str = "test-trusted-host-secret";
const SERVICE_CLIENT_ID: &str = "test-service-client";
const SERVICE_CLIENT_SECRET: &str = "test-private-client-secret";
const SERVICE_TOKEN: &str = "test-private-service-token";

async fn build_trusted_submit_app(base_url: String) -> (axum::Router, AppServices, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let database = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: root.path().to_owned(),
        work_dir: root.path().to_owned(),
        bootstrap_secret: Some(HOST_SECRET.to_owned()),
        ..AppConfig::default()
    };
    let services = AppServices::from_config(database, &config).await.unwrap();
    let (mut states, _) = build_module_states(&services).await.unwrap();
    states.gea = aionui_gea::GeaRouterState::new(
        aionui_gea::GeaService::new(reqwest::Client::new(), base_url)
            .unwrap()
            .with_sales_plan_service_identity_for_test(SERVICE_CLIENT_ID, SERVICE_CLIENT_SECRET),
    );
    // Inject the host capability through the real app route composition, not
    // directly into this overridden domain state.
    let app = create_router_with_states(&services, states);
    (app, services, root)
}

fn valid_submit_body() -> serde_json::Value {
    serde_json::json!({
        "periodId": "202608",
        "periodMonth": "2026-08",
        "planTypeCode": "monthly",
        "channelCode": "gea",
        "dealerCode": "1001",
        "targetQty": "1.000",
        "targetAmount": "1.00",
        "submitterCode": "aioncore",
        "items": [{
            "skuCode": "9007199254740993",
            "productCategName": "产品",
            "baseQty": "1.000",
            "qty": "1.000",
            "price": "1.00"
        }]
    })
}

#[tokio::test]
async fn trusted_submit_routes_reach_service_identity_only_after_user_and_host_authorization() {
    let upstream = MockServer::start().await;
    let expected_receipt = serde_json::json!({
        "planId": "9007199254740993", "versionId": "9007199254740995",
        "seq": 1, "status": 1, "replayed": false,
        "requestId": "request-positive", "traceId": "trace-positive", "auditId": "audit-positive"
    });
    let expected_upstream: serde_json::Value = serde_json::from_str(
        r#"{
            "periodId": 202608, "periodMonth": "2026-08", "planTypeCode": "monthly",
            "channelCode": "gea", "dealerCode": 1001,
            "targetQty": 26445.000, "targetAmount": 1539999.99,
            "submitterCode": "aioncore",
            "items": [{"skuCode": 9007199254740993, "productCategName": "产品",
                       "baseQty": 1.000, "qty": 1.000, "price": 1.00}]
        }"#,
    )
    .unwrap();
    Mock::given(method("POST"))
        .and(path("/api/v1/internal/auth/token"))
        .and(matches_header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string_contains("grant_type=client_credentials"))
        .and(body_string_contains(format!("client_id={SERVICE_CLIENT_ID}")))
        .and(body_string_contains(format!("client_secret={SERVICE_CLIENT_SECRET}")))
        .and(body_string_contains("scope=sales-plan%3Awrite"))
        .and(body_string_contains("requested_ttl_seconds=600"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "code": 200,
            "result": {"access_token": SERVICE_TOKEN, "token_type": "Bearer",
                       "expires_in": 600, "scope": "sales-plan:write"}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/internal/sales-plans"))
        .and(matches_header("authorization", format!("Bearer {SERVICE_TOKEN}")))
        .and(matches_header("idempotency-key", "submit-positive"))
        .and(matches_header("x-request-id", "request-positive"))
        .and(matches_body_json(expected_upstream))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "success": true, "code": 200, "result": expected_receipt.clone()
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let (mut app, services, _root) = build_trusted_submit_app(upstream.uri()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let mut body = valid_submit_body();
    body["targetQty"] = serde_json::json!("26445.000");
    body["targetAmount"] = serde_json::json!("1539999.99");

    let mut without_host = json_with_token("POST", "/api/gea/sales-plan/submissions", body.clone(), &token, &csrf);
    without_host
        .headers_mut()
        .insert("idempotency-key", "submit-denied".parse().unwrap());
    without_host
        .headers_mut()
        .insert("x-request-id", "request-denied".parse().unwrap());
    let response = app.clone().oneshot(without_host).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["code"],
        "GEA_SALES_PLAN_SUBMIT_CAPABILITY_REQUIRED"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());

    let user = services.user_repo.find_by_username("admin").await.unwrap().unwrap();
    let runtime = services.runtime_token_service.issue(
        &user.id,
        "submit-runtime-conversation",
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );
    let runtime_request = Request::builder()
        .method("POST")
        .uri("/api/gea/sales-plan/submissions")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-aionui-runtime-token", &runtime.token)
        .header("x-aionui-user-id", &user.id)
        .header("x-aionui-conversation-id", "submit-runtime-conversation")
        .header("x-aioncore-bootstrap-secret", HOST_SECRET)
        .header("idempotency-key", "submit-runtime-denied")
        .header("x-request-id", "request-runtime-denied")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.clone().oneshot(runtime_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(response).await["code"],
        "GEA_AUTH_SESSION_TRUSTED_CLIENT_REQUIRED"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());

    let mut request = json_with_token("POST", "/api/gea/sales-plan/submissions", body, &token, &csrf);
    request
        .headers_mut()
        .insert("x-aioncore-bootstrap-secret", HOST_SECRET.parse().unwrap());
    request
        .headers_mut()
        .insert("idempotency-key", "submit-positive".parse().unwrap());
    request
        .headers_mut()
        .insert("x-request-id", "request-positive".parse().unwrap());
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = body_json(response).await;
    assert_eq!(response["success"], true);
    assert_eq!(response["data"], expected_receipt);
    for secret in [HOST_SECRET, SERVICE_CLIENT_SECRET, SERVICE_TOKEN, token.as_str()] {
        assert!(!response.to_string().contains(secret), "receipt leaked credentials");
    }

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one token exchange and one write after authorization"
    );
    for request in requests {
        assert!(request.headers.get("x-aioncore-bootstrap-secret").is_none());
        assert!(request.headers.get("cookie").is_none());
        assert!(request.headers.get("x-aionui-runtime-token").is_none());
        assert!(!String::from_utf8_lossy(&request.body).contains(HOST_SECRET));
    }
    services.database.close().await;
}

#[tokio::test]
async fn sales_plan_routes_enforce_auth_csrf_and_trusted_submit_capability() {
    let (mut app, services) = build_app().await;

    let response = app
        .clone()
        .oneshot(get_request("/api/gea/sales-plan/periods"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/gea/sales-plan/submissions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("idempotency-key", "submit-key-1")
                .header("x-request-id", "request-1")
                .body(Body::from(serde_json::to_vec(&valid_submit_body()).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");

    let mut request = json_with_token(
        "POST",
        "/api/gea/sales-plan/submissions",
        valid_submit_body(),
        &token,
        &csrf,
    );
    request
        .headers_mut()
        .insert("idempotency-key", "submit-key-1".parse().unwrap());
    request
        .headers_mut()
        .insert("x-request-id", "request-1".parse().unwrap());
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["code"], "GEA_SALES_PLAN_SUBMIT_CAPABILITY_REQUIRED");
    let serialized = body.to_string();
    assert!(!serialized.contains("bootstrap-secret"));
    assert!(!serialized.contains("client-secret"));
    assert!(!serialized.contains("service-token"));

    services.database.close().await;
}

#[tokio::test]
async fn sales_plan_write_routes_use_authenticated_action_rate_limit() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    for attempt in 0..20 {
        let mut request = json_with_token(
            "POST",
            "/api/gea/sales-plan/submissions",
            valid_submit_body(),
            &token,
            &csrf,
        );
        request
            .headers_mut()
            .insert("idempotency-key", format!("submit-key-{attempt}").parse().unwrap());
        request
            .headers_mut()
            .insert("x-request-id", format!("request-{attempt}").parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "attempt {attempt}");
    }

    let mut request = json_with_token(
        "POST",
        "/api/gea/sales-plan/submissions",
        valid_submit_body(),
        &token,
        &csrf,
    );
    request
        .headers_mut()
        .insert("idempotency-key", "submit-key-limited".parse().unwrap());
    request
        .headers_mut()
        .insert("x-request-id", "request-limited".parse().unwrap());
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(response).await["code"], "RATE_LIMITED");

    services.database.close().await;
}

#[tokio::test]
async fn sales_plan_preflight_allows_required_contract_headers() {
    let (app, services) = build_app().await;
    let response = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/gea/sales-plan/submissions")
                .header(header::ORIGIN, "http://localhost:5173")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,content-type,x-csrf-token,idempotency-key,x-request-id,x-trace-id",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    let allowed = response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    for expected in ["idempotency-key", "x-request-id", "x-trace-id"] {
        assert!(allowed.contains(expected), "missing {expected} in {allowed}");
    }

    services.database.close().await;
}
