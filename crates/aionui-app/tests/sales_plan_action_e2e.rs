mod common;

use axum::body::Body;
use axum::http::Request;
use axum::http::{HeaderValue, StatusCode};
use tower::ServiceExt;

use common::{body_json, build_app, get_request, json_with_token, setup_and_login};

fn sales_plan_action_request(token: &str, csrf: &str) -> axum::http::Request<axum::body::Body> {
    let mut request = json_with_token(
        "POST",
        "/api/gea/sales-plan/plans/versions/version-1/actions",
        serde_json::json!({"action": "APPROVE", "expectedStatus": 2}),
        token,
        csrf,
    );
    request
        .headers_mut()
        .insert("idempotency-key", HeaderValue::from_static("action-key-1"));
    request
        .headers_mut()
        .insert("x-request-id", HeaderValue::from_static("request-1"));
    request
}

#[tokio::test]
async fn sales_plan_routes_require_authentication() {
    let (app, _) = build_app().await;
    let response = app.oneshot(get_request("/api/gea/sales-plan/periods")).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn sales_plan_actions_require_csrf() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "sales-plan-csrf-user", "StrongP@ss1").await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/gea/sales-plan/plans/versions/version-1/actions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("idempotency-key", "action-key-1")
        .header("x-request-id", "request-1")
        .body(Body::from(r#"{"action":"APPROVE","expectedStatus":2}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn sales_plan_actions_are_rate_limited_per_authenticated_user() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "sales-plan-user", "StrongP@ss1").await;

    for attempt in 0..20 {
        let response = app
            .clone()
            .oneshot(sales_plan_action_request(&token, &csrf))
            .await
            .unwrap();
        let status = response.status();
        let body = body_json(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}: {body}");
    }

    let response = app.oneshot(sales_plan_action_request(&token, &csrf)).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(response).await["code"], "RATE_LIMITED");
}
