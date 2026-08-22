//! Full-router checks for the debug-only GEA OpenAPI documentation surface.

#![cfg(debug_assertions)]

mod common;

use axum::body::to_bytes;
use axum::http::StatusCode;
use tower::ServiceExt;

use common::{body_json, build_app, get_request, get_with_token, setup_and_login};

#[tokio::test]
async fn gea_openapi_requires_aioncore_authentication() {
    let (app, _) = build_app().await;
    let response = app.oneshot(get_request("/openapi.json")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authenticated_user_can_read_the_gea_openapi_document() {
    let (mut app, services) = build_app().await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(get_with_token("/openapi.json", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let document = body_json(response).await;
    assert_eq!(document["info"]["title"], "AionCore GEA API");
    assert!(document["paths"]["/api/gea/auth/session"].is_object());
    assert!(document["components"]["securitySchemes"]["bearerAuth"].is_object());
    assert!(document["components"]["securitySchemes"]["sessionCookie"].is_object());
    assert!(document["components"]["securitySchemes"]["runtimeToken"].is_object());

    let response = app.oneshot(get_with_token("/swagger-ui/", &token)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("Swagger UI"));
}
