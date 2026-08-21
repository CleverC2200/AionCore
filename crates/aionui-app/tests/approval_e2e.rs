mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

use aionui_api_types::{ApprovalActionReceiptStatus, ApprovalTaskActionRequest};
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use aionui_approval::ApprovalService;
use common::{body_json, get_request, get_with_token, json_with_token, setup_and_login};

async fn build_app_with_approval_responses(responses: Vec<serde_json::Value>) -> (axum::Router, AppServices) {
    let database = aionui_db::init_database_memory().await.unwrap();
    let mut services = AppServices::from_config(database, &AppConfig::default()).await.unwrap();
    services.approval_service = ApprovalService::from_test_responses(
        responses,
        Arc::new(aionui_db::SqliteApprovalReceiptRepository::new(
            services.database.pool().clone(),
        )),
        true,
    );
    let (states, _) = build_module_states(&services).await.expect("build module states");
    let router = create_router_with_states(&services, states);
    (router, services)
}

#[tokio::test]
async fn approval_routes_require_authentication_and_csrf() {
    let (mut app, services) = build_app_with_approval_responses(vec![]).await;

    let response = app
        .clone()
        .oneshot(get_request("/api/approvals/tasks?topic=pending"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (token, _csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/approvals/tasks/approve")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::json!({
                "instanceCode": "instance-1",
                "taskId": "task-1",
                "idempotencyKey": "intent-1"
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["code"], "CSRF_INVALID");
}

#[tokio::test]
async fn approval_write_rejects_invalid_payload_after_security_gates() {
    let (mut app, services) = build_app_with_approval_responses(vec![]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let request = json_with_token(
        "POST",
        "/api/approvals/tasks/approve",
        serde_json::json!({ "instanceCode": "", "taskId": "", "idempotencyKey": "" }),
        &token,
        &csrf,
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["code"], "APPROVAL_INVALID_REQUEST");
}

#[tokio::test]
async fn approval_list_normal_flow_returns_normalized_provider_data() {
    let (mut app, services) = build_app_with_approval_responses(vec![serde_json::json!({
        "count": 1,
        "has_more": false,
        "tasks": [{
            "task_id": "task-1",
            "instance_code": "instance-1",
            "definition_code": "definition-1",
            "definition_name": "需求预测测试",
            "title": "2026年9月需求预测",
            "topic": 1,
            "status": 1,
            "instance_status": 1,
            "user_id": "ou_owner",
            "support_api_operate": true,
            "summaries": [{"key": "事项说明", "value": "计划提报"}]
        }]
    })])
    .await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let response = app
        .oneshot(get_with_token("/api/approvals/tasks?topic=pending", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["tasks"][0]["taskId"], "task-1");
    assert_eq!(body["data"]["tasks"][0]["supportApiOperate"], true);
}

#[tokio::test]
async fn approval_instance_normal_flow_returns_form_and_workflow_data() {
    let (mut app, services) = build_app_with_approval_responses(vec![serde_json::json!({
        "instance_code": "instance-1",
        "definition_code": "definition-1",
        "definition_name": "需求预测测试",
        "serial_number": "202608200032",
        "status": "PENDING",
        "start_time": "1787184000000",
        "end_time": "0",
        "user_id": "ou_initiator",
        "form": serde_json::to_string(&serde_json::json!([{
            "id": "description",
            "name": "事项说明",
            "type": "textarea",
            "value": "计划提报"
        }])).unwrap(),
        "current_nodes": [],
        "tasks": [{
            "id": "task-1",
            "user_id": "ou_owner",
            "status": "PENDING",
            "start_time": "1787184000000",
            "end_time": "0"
        }],
        "operation_records": [{"type": "START", "create_time": "1787184000000"}],
        "comments": []
    })])
    .await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let response = app
        .oneshot(get_with_token("/api/approvals/instances/instance-1", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["form"][0]["name"], "事项说明");
    assert_eq!(body["data"]["tasks"][0]["id"], "task-1");
    assert_eq!(body["data"]["operations"][0]["operationType"], "START");
}

#[tokio::test]
async fn approval_write_normal_flow_returns_a_durable_receipt() {
    let (mut app, services) = build_app_with_approval_responses(vec![serde_json::json!({})]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let request = json_with_token(
        "POST",
        "/api/approvals/tasks/approve",
        serde_json::json!({
            "instanceCode": "instance-1",
            "taskId": "task-1",
            "idempotencyKey": "intent-success"
        }),
        &token,
        &csrf,
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["status"], "succeeded");
    assert_eq!(body["data"]["idempotencyKey"], "intent-success");
}

#[tokio::test]
async fn approval_reject_normal_flow_returns_a_durable_receipt() {
    let (mut app, services) = build_app_with_approval_responses(vec![serde_json::json!({})]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let request = json_with_token(
        "POST",
        "/api/approvals/tasks/reject",
        serde_json::json!({
            "instanceCode": "instance-1",
            "taskId": "task-1",
            "comment": "数据依据不足",
            "idempotencyKey": "intent-reject"
        }),
        &token,
        &csrf,
    );
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["status"], "succeeded");
    assert_eq!(body["data"]["idempotencyKey"], "intent-reject");
}

#[tokio::test]
async fn approval_contact_search_then_transfer_normal_flow_succeeds() {
    let (mut app, services) = build_app_with_approval_responses(vec![
        serde_json::json!({
            "users": [{
                "open_id": "ou_target",
                "localized_name": "王审批",
                "department": "数智化部",
                "is_cross_tenant": false
            }]
        }),
        serde_json::json!({}),
    ])
    .await;
    let (token, csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(get_with_token("/api/approvals/contacts?query=wang", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"][0]["openId"], "ou_target");

    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/approvals/tasks/transfer",
            serde_json::json!({
                "instanceCode": "instance-1",
                "taskId": "task-1",
                "transferUserId": "ou_target",
                "idempotencyKey": "intent-transfer-success"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["data"]["status"], "succeeded");
    assert_eq!(body["data"]["taskId"], "task-1");
}

#[tokio::test]
async fn approval_writes_are_rate_limited_per_authenticated_user() {
    let (mut app, services) = build_app_with_approval_responses(vec![serde_json::json!({})]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "approval-admin", "StrongP@ss1").await;
    for attempt in 0..20 {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/approvals/tasks/approve",
                serde_json::json!({
                    "instanceCode": "instance-1",
                    "taskId": "task-1",
                    "idempotencyKey": "intent-rate-limit"
                }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "attempt {attempt}");
    }
    let response = app
        .oneshot(json_with_token(
            "POST",
            "/api/approvals/tasks/approve",
            serde_json::json!({
                "instanceCode": "instance-1",
                "taskId": "task-1",
                "idempotencyKey": "intent-rate-limit"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(response).await["code"], "RATE_LIMITED");
}

#[tokio::test]
async fn approval_receipt_replays_after_service_restart_without_calling_the_provider() {
    let database = aionui_db::init_database_memory().await.unwrap();
    let receipt_repo = Arc::new(aionui_db::SqliteApprovalReceiptRepository::new(database.pool().clone()));
    let request = ApprovalTaskActionRequest {
        instance_code: "instance-1".to_owned(),
        task_id: "task-1".to_owned(),
        comment: None,
        idempotency_key: "intent-restart-success".to_owned(),
    };
    let first = ApprovalService::from_test_responses(vec![serde_json::json!({})], receipt_repo.clone(), true)
        .approve(request.clone())
        .await
        .expect("first write");
    assert_eq!(first.status, ApprovalActionReceiptStatus::Succeeded);

    let replay = ApprovalService::from_test_responses(vec![], receipt_repo, true)
        .approve(request)
        .await
        .expect("durable replay");
    assert_eq!(replay, first);
}
