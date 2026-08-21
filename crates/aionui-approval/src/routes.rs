#![allow(clippy::disallowed_types)]

use aionui_api_types::{
    ApiResponse, ApprovalActionReceipt, ApprovalContact, ApprovalInstance, ApprovalList, ApprovalListTopic,
    ApprovalTaskActionRequest, ApprovalTaskTransferRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::{ApprovalError, ApprovalRouterState, ApprovalUpstreamError};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListQuery {
    #[serde(default = "pending_topic")]
    topic: ApprovalListTopic,
    #[serde(default = "default_page_size")]
    page_size: u16,
    definition_code: Option<String>,
    page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContactQuery {
    query: String,
}

fn pending_topic() -> ApprovalListTopic {
    ApprovalListTopic::Pending
}

fn default_page_size() -> u16 {
    50
}

pub fn approval_routes(state: ApprovalRouterState) -> Router {
    approval_read_routes(state.clone()).merge(approval_action_routes(state))
}

pub fn approval_read_routes(state: ApprovalRouterState) -> Router {
    Router::new()
        .route("/api/approvals/tasks", get(list_tasks))
        .route("/api/approvals/contacts", get(search_contacts))
        .route("/api/approvals/instances/{instance_code}", get(get_instance))
        .with_state(state)
}

pub fn approval_action_routes(state: ApprovalRouterState) -> Router {
    Router::new()
        .route("/api/approvals/tasks/approve", post(approve_task))
        .route("/api/approvals/tasks/reject", post(reject_task))
        .route("/api/approvals/tasks/transfer", post(transfer_task))
        .with_state(state)
}

async fn search_contacts(
    State(state): State<ApprovalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ContactQuery>,
) -> Result<Json<ApiResponse<Vec<ApprovalContact>>>, ApiError> {
    let result = state
        .service
        .search_contacts(&query.query)
        .await
        .map_err(map_approval_error)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn list_tasks(
    State(state): State<ApprovalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ApiResponse<ApprovalList>>, ApiError> {
    let page_size = query.page_size.clamp(1, 100);
    let result = state
        .service
        .list_tasks(
            query.topic,
            page_size,
            query.definition_code.as_deref(),
            query.page_token.as_deref(),
        )
        .await
        .map_err(map_approval_error)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn get_instance(
    State(state): State<ApprovalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    Path(instance_code): Path<String>,
) -> Result<Json<ApiResponse<ApprovalInstance>>, ApiError> {
    let result = state
        .service
        .get_instance(&instance_code)
        .await
        .map_err(map_approval_error)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn approve_task(
    State(state): State<ApprovalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ApprovalTaskActionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ApprovalActionReceipt>>, ApiError> {
    let Json(request) = body.map_err(|_| map_approval_error(ApprovalError::invalid("审批参数无效")))?;
    let receipt = state.service.approve(request).await.map_err(map_approval_error)?;
    Ok(Json(ApiResponse::ok(receipt)))
}

async fn reject_task(
    State(state): State<ApprovalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ApprovalTaskActionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ApprovalActionReceipt>>, ApiError> {
    let Json(request) = body.map_err(|_| map_approval_error(ApprovalError::invalid("驳回参数无效")))?;
    let receipt = state.service.reject(request).await.map_err(map_approval_error)?;
    Ok(Json(ApiResponse::ok(receipt)))
}

async fn transfer_task(
    State(state): State<ApprovalRouterState>,
    Extension(_user): Extension<CurrentUser>,
    body: Result<Json<ApprovalTaskTransferRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ApprovalActionReceipt>>, ApiError> {
    let Json(request) = body.map_err(|_| map_approval_error(ApprovalError::invalid("转交参数无效")))?;
    let receipt = state.service.transfer(request).await.map_err(map_approval_error)?;
    Ok(Json(ApiResponse::ok(receipt)))
}

fn map_approval_error(error: ApprovalError) -> ApiError {
    let (status, code, message) = match error {
        ApprovalError::Invalid(message) => (StatusCode::BAD_REQUEST, "APPROVAL_INVALID_REQUEST", message),
        ApprovalError::TrustedClientRequired => (
            StatusCode::FORBIDDEN,
            "APPROVAL_TRUSTED_CLIENT_REQUIRED",
            "飞书审批仅支持本机受信客户端".to_owned(),
        ),
        ApprovalError::ProviderUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "APPROVAL_PROVIDER_UNAVAILABLE",
            "飞书审批连接不可用，请检查 lark-cli 安装与登录状态".to_owned(),
        ),
        ApprovalError::InvalidProviderResponse => (
            StatusCode::BAD_GATEWAY,
            "APPROVAL_UPSTREAM_ERROR",
            "飞书审批返回结构无效".to_owned(),
        ),
        ApprovalError::Upstream(ApprovalUpstreamError::StaleTask) => (
            StatusCode::CONFLICT,
            "APPROVAL_UPSTREAM_ERROR",
            "审批任务已变化，请刷新后重试".to_owned(),
        ),
        ApprovalError::Upstream(ApprovalUpstreamError::Permission) => (
            StatusCode::FORBIDDEN,
            "APPROVAL_UPSTREAM_ERROR",
            "当前飞书账号缺少审批操作权限".to_owned(),
        ),
        ApprovalError::Upstream(ApprovalUpstreamError::Other) => (
            StatusCode::BAD_GATEWAY,
            "APPROVAL_UPSTREAM_ERROR",
            "飞书审批请求失败".to_owned(),
        ),
        ApprovalError::IdempotencyConflict => (
            StatusCode::CONFLICT,
            "APPROVAL_IDEMPOTENCY_CONFLICT",
            "同一幂等键不能用于不同审批操作".to_owned(),
        ),
        ApprovalError::StorageUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "APPROVAL_RECEIPT_STORAGE_UNAVAILABLE",
            "审批回执暂时不可用，请勿重复提交".to_owned(),
        ),
    };
    ApiError::coded(status, code, message, None)
}
