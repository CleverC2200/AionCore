use aionui_api_types::{
    ApiResponse, CreateGeaSessionRequest, ErrorResponse, GeaAuthSessionStatus, GeaClientResourceSyncResult,
    GeaInteractionRequestActionCommand, GeaInteractionRequestReceipt, GeaInteractionRequestSnapshot,
    GeaResourceContents, GeaResourceList, GeaResourceTemplateList, GeaSessionResponse, GeaToolCallRequest,
    GeaToolCallResponse, GeaToolInfo, InteractionRequestActionCommand, InteractionRequestList,
    InteractionRequestReceipt, NotificationActionCommand, NotificationList, NotificationReceipt, NotificationView,
    ReadGeaResourceRequest, SetGeaAuthSessionRequest, SyncGeaClientResourcesRequest,
};
#[cfg(debug_assertions)]
use aionui_api_types::{
    GeaNotification, GeaNotificationReceipt, GeaNotificationSnapshot, InteractionRequestChangedPayload,
    NotificationChangedPayload, NotificationTarget, WebSocketMessage,
};
use aionui_auth::{CurrentUser, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use serde::Deserialize;
#[cfg(debug_assertions)]
use utoipa::OpenApi;
#[cfg(debug_assertions)]
use utoipa::openapi::Components;
#[cfg(debug_assertions)]
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
#[cfg(debug_assertions)]
use utoipa_swagger_ui::{Config as SwaggerConfig, SwaggerUi};

use crate::error::GeaError;
use crate::state::GeaRouterState;

pub fn gea_routes(state: GeaRouterState) -> Router {
    let router = Router::new()
        .route(
            "/api/gea/auth/session",
            get(auth_status).put(set_auth_session).delete(clear_auth_session),
        )
        .route("/api/gea/conversations/{conversation_id}/session", post(create_session))
        .route("/api/gea/conversations/{conversation_id}/tools", get(list_tools))
        .route(
            "/api/gea/conversations/{conversation_id}/resources",
            get(list_resources),
        )
        .route(
            "/api/gea/conversations/{conversation_id}/resource-templates",
            get(list_resource_templates),
        )
        .route(
            "/api/gea/conversations/{conversation_id}/resources/read",
            post(read_resource),
        )
        .route("/api/gea/mcp/test", post(test_mcp_connection))
        .route(
            "/api/gea/conversations/{conversation_id}/interaction-requests",
            get(list_interaction_requests),
        )
        .route(
            "/api/gea/conversations/{conversation_id}/interaction-requests/{request_id}/actions",
            post(act_on_interaction_request),
        )
        .route("/api/interaction-requests", get(list_all_interaction_requests))
        .route(
            "/api/interaction-requests/{request_id}/actions",
            post(act_on_global_interaction_request),
        )
        .route("/api/notifications", get(list_notifications))
        .route("/api/notifications/{notification_id}", get(get_notification))
        .route(
            "/api/notifications/{notification_id}/read",
            post(mark_notification_read),
        )
        .route(
            "/api/notifications/{notification_id}/dismiss",
            post(dismiss_notification),
        )
        .route("/api/client-resources/sync", post(sync_client_resources))
        .route(
            "/api/gea/conversations/{conversation_id}/tools/{tool_name}",
            post(call_tool),
        )
        .with_state(state);

    #[cfg(debug_assertions)]
    {
        router.merge(gea_api_docs_routes())
    }

    #[cfg(not(debug_assertions))]
    {
        router
    }
}

#[cfg(debug_assertions)]
fn gea_api_docs_routes() -> Router {
    Router::new().merge(
        SwaggerUi::new("/swagger-ui")
            .url("/openapi.json", GeaApiDoc::openapi())
            .config(gea_swagger_config()),
    )
}

#[cfg(debug_assertions)]
fn gea_swagger_config() -> SwaggerConfig<'static> {
    SwaggerConfig::new(["/openapi.json"])
        .supported_submit_methods(std::iter::empty::<String>())
        .validator_url("none")
}

#[utoipa::path(
    post,
    path = "/api/client-resources/sync",
    operation_id = "syncGeaClientResources",
    tag = "Client resources",
    request_body(
        content = SyncGeaClientResourcesRequest,
        example = json!({"resources": ["skills"]})
    ),
    params(
        ("x-csrf-token" = Option<String>, Header, description = "Required for state-changing requests outside local identity mode; must match the CSRF cookie")
    ),
    responses(
        (status = 200, description = "Synchronization result", body = ApiResponse<GeaClientResourceSyncResult>),
        (status = 400, description = "Invalid resource list", body = ErrorResponse),
        (status = 401, description = "AionCore authentication required", body = ErrorResponse),
        (status = 403, description = "CSRF validation failed or runtime credential is not allowed", body = ErrorResponse),
        (status = 500, description = "Resource storage is unavailable", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid or failed response", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn sync_client_resources(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    body: Result<Json<SyncGeaClientResourcesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<GeaClientResourceSyncResult>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    let Json(request) = body.map_err(|_| GeaError::invalid_request("GEA 资源同步参数无效"))?;
    let result = state.service.sync_client_resources(&user.id, request).await?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(Debug, Deserialize)]
struct InteractionRequestListQuery {
    #[serde(default = "active_status")]
    status: String,
}

fn active_status() -> String {
    "active".to_owned()
}

#[utoipa::path(
    get,
    path = "/api/interaction-requests",
    operation_id = "listInteractionRequests",
    tag = "InteractionRequest",
    params(
        ("status" = Option<String>, Query, description = "Current implementation accepts active and the pending compatibility value; default is active")
    ),
    responses(
        (status = 200, description = "User-scoped recoverable projection", body = ApiResponse<InteractionRequestList>),
        (status = 400, description = "Unsupported status filter", body = ErrorResponse),
        (status = 401, description = "AionCore authentication required", body = ErrorResponse),
        (status = 500, description = "Projection storage is unavailable", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn list_all_interaction_requests(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<InteractionRequestListQuery>,
) -> Result<Json<ApiResponse<InteractionRequestList>>, GeaError> {
    if !matches!(query.status.as_str(), "active" | "pending") {
        return Err(GeaError::invalid_request("当前只支持 status=active"));
    }
    let snapshot = state.service.list_all_interaction_requests(&user.id).await?;
    Ok(Json(ApiResponse::ok(snapshot)))
}

#[utoipa::path(
    post,
    path = "/api/interaction-requests/{request_id}/actions",
    operation_id = "actOnInteractionRequest",
    tag = "InteractionRequest",
    request_body(
        content = InteractionRequestActionCommand,
        example = json!({
            "expected_version": "v1",
            "idempotency_key": "example-command-1",
            "action_id": "answer",
            "payload": {"answers": [{"questionIndex": 0, "selectedLabels": ["Example"]}]}
        })
    ),
    params(
        ("request_id" = String, Path, description = "InteractionRequest identifier"),
        ("x-csrf-token" = Option<String>, Header, description = "Required for state-changing requests outside local identity mode; must match the CSRF cookie")
    ),
    responses(
        (status = 200, description = "Stable action receipt", body = ApiResponse<InteractionRequestReceipt>),
        (status = 400, description = "Invalid action command", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "CSRF validation or ownership check failed", body = ErrorResponse),
        (status = 404, description = "InteractionRequest not found", body = ErrorResponse),
        (status = 409, description = "Version, replay, or turn continuation conflict", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid or failed response", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn act_on_global_interaction_request(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(request_id): Path<String>,
    body: Result<Json<InteractionRequestActionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<InteractionRequestReceipt>>, GeaError> {
    let Json(command) = body.map_err(|_| GeaError::invalid_request("Interaction Request 动作参数无效"))?;
    let receipt = state
        .service
        .act_on_global_interaction_request(&user.id, &request_id, command)
        .await?;
    Ok(Json(ApiResponse::ok(receipt)))
}

#[derive(Debug, Deserialize)]
struct NotificationListQuery {
    #[serde(default = "active_status")]
    status: String,
}

#[utoipa::path(
    get,
    path = "/api/notifications",
    operation_id = "listNotifications",
    tag = "Notification",
    params(
        ("status" = Option<String>, Query, description = "active, unread, read, dismissed, or all; default is active")
    ),
    responses(
        (status = 200, description = "Tenant-scoped recoverable notification projection", body = ApiResponse<NotificationList>),
        (status = 400, description = "Unsupported status filter", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 429, description = "GEA rate limit", body = ErrorResponse),
        (status = 500, description = "Projection storage is unavailable", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid response", body = ErrorResponse),
        (status = 503, description = "GEA is temporarily unavailable", body = ErrorResponse)
    ),
    security(("bearerAuth" = []), ("sessionCookie" = []))
)]
async fn list_notifications(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<NotificationListQuery>,
) -> Result<Json<ApiResponse<NotificationList>>, GeaError> {
    if !matches!(
        query.status.as_str(),
        "active" | "unread" | "read" | "dismissed" | "all"
    ) {
        return Err(GeaError::invalid_request("Notification status 筛选无效"));
    }
    let list = state.service.list_notifications(&user.id, Some(&query.status)).await?;
    Ok(Json(ApiResponse::ok(list)))
}

#[utoipa::path(
    get,
    path = "/api/notifications/{notification_id}",
    operation_id = "getNotification",
    tag = "Notification",
    params(("notification_id" = String, Path, description = "Notification identifier")),
    responses(
        (status = 200, description = "Notification detail", body = ApiResponse<NotificationView>),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 404, description = "Notification not found in the current user and tenant scope", body = ErrorResponse)
    ),
    security(("bearerAuth" = []), ("sessionCookie" = []))
)]
async fn get_notification(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(notification_id): Path<String>,
) -> Result<Json<ApiResponse<NotificationView>>, GeaError> {
    let notification = state.service.get_notification(&user.id, &notification_id).await?;
    Ok(Json(ApiResponse::ok(notification)))
}

#[utoipa::path(
    post,
    path = "/api/notifications/{notification_id}/read",
    operation_id = "markNotificationRead",
    tag = "Notification",
    request_body = NotificationActionCommand,
    params(
        ("notification_id" = String, Path, description = "Notification identifier"),
        ("x-csrf-token" = Option<String>, Header, description = "Required outside local identity mode")
    ),
    responses(
        (status = 200, description = "Stable read receipt", body = ApiResponse<NotificationReceipt>),
        (status = 400, description = "Invalid action command", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "CSRF validation or ownership check failed", body = ErrorResponse),
        (status = 404, description = "Notification not found", body = ErrorResponse),
        (status = 409, description = "Version or state conflict", body = ErrorResponse),
        (status = 422, description = "Notification action is not valid for the current state", body = ErrorResponse),
        (status = 429, description = "GEA rate limit", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid response", body = ErrorResponse),
        (status = 503, description = "GEA is temporarily unavailable", body = ErrorResponse)
    ),
    security(("bearerAuth" = []), ("sessionCookie" = []))
)]
async fn mark_notification_read(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(notification_id): Path<String>,
    body: Result<Json<NotificationActionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<NotificationReceipt>>, GeaError> {
    let Json(command) = body.map_err(|_| GeaError::invalid_request("Notification 已读参数无效"))?;
    let receipt = state
        .service
        .mark_notification_read(&user.id, &notification_id, command)
        .await?;
    Ok(Json(ApiResponse::ok(receipt)))
}

#[utoipa::path(
    post,
    path = "/api/notifications/{notification_id}/dismiss",
    operation_id = "dismissNotification",
    tag = "Notification",
    request_body = NotificationActionCommand,
    params(
        ("notification_id" = String, Path, description = "Notification identifier"),
        ("x-csrf-token" = Option<String>, Header, description = "Required outside local identity mode")
    ),
    responses(
        (status = 200, description = "Stable dismiss receipt", body = ApiResponse<NotificationReceipt>),
        (status = 400, description = "Invalid action command", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "CSRF validation or ownership check failed", body = ErrorResponse),
        (status = 404, description = "Notification not found", body = ErrorResponse),
        (status = 409, description = "Version, state, or dismissible conflict", body = ErrorResponse),
        (status = 422, description = "Notification action is not valid for the current state", body = ErrorResponse),
        (status = 429, description = "GEA rate limit", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid response", body = ErrorResponse),
        (status = 503, description = "GEA is temporarily unavailable", body = ErrorResponse)
    ),
    security(("bearerAuth" = []), ("sessionCookie" = []))
)]
async fn dismiss_notification(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(notification_id): Path<String>,
    body: Result<Json<NotificationActionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<NotificationReceipt>>, GeaError> {
    let Json(command) = body.map_err(|_| GeaError::invalid_request("Notification 关闭参数无效"))?;
    let receipt = state
        .service
        .dismiss_notification(&user.id, &notification_id, command)
        .await?;
    Ok(Json(ApiResponse::ok(receipt)))
}

#[utoipa::path(
    put,
    path = "/api/gea/auth/session",
    operation_id = "setGeaAuthSession",
    tag = "GEA session",
    request_body(
        content = SetGeaAuthSessionRequest,
        example = json!({"accessToken": "<GEA_ACCESS_TOKEN>", "tenantId": "tenant-example"})
    ),
    params(
        ("x-csrf-token" = Option<String>, Header, description = "Required outside local identity mode; must match the CSRF cookie")
    ),
    responses(
        (status = 200, description = "GEA authentication status; the access token is never returned", body = ApiResponse<GeaAuthSessionStatus>),
        (status = 400, description = "Invalid GEA session payload", body = ErrorResponse),
        (status = 401, description = "AionCore authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credentials cannot manage desktop GEA credentials, or CSRF failed", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn set_auth_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Json(request): Json<SetGeaAuthSessionRequest>,
) -> Result<Json<ApiResponse<GeaAuthSessionStatus>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    let status = state.service.set_auth_session(&user.id, request).await?;
    Ok(Json(ApiResponse::ok(status)))
}

#[utoipa::path(
    get,
    path = "/api/gea/auth/session",
    operation_id = "getGeaAuthSession",
    tag = "GEA session",
    responses(
        (status = 200, description = "GEA authentication status; no credential is returned", body = ApiResponse<GeaAuthSessionStatus>),
        (status = 401, description = "AionCore authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credentials cannot inspect desktop GEA credentials", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn auth_status(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<GeaAuthSessionStatus>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    Ok(Json(ApiResponse::ok(state.service.auth_status(&user.id).await)))
}

#[utoipa::path(
    delete,
    path = "/api/gea/auth/session",
    operation_id = "clearGeaAuthSession",
    tag = "GEA session",
    params(
        ("x-csrf-token" = Option<String>, Header, description = "Required outside local identity mode; must match the CSRF cookie")
    ),
    responses(
        (status = 200, description = "GEA session cleared", body = ApiResponse<utoipa::TupleUnit>),
        (status = 401, description = "AionCore authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credentials cannot clear desktop GEA credentials, or CSRF failed", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn clear_auth_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<()>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    state.service.clear_auth_session(&user.id).await;
    Ok(Json(ApiResponse::success()))
}

#[utoipa::path(
    post,
    path = "/api/gea/conversations/{conversation_id}/session",
    operation_id = "createGeaConversationSession",
    tag = "GEA session",
    request_body(
        content = CreateGeaSessionRequest,
        example = json!({"consumerCode": "sales_forecast"})
    ),
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("x-aionui-runtime-token" = Option<String>, Header, description = "Conversation helper credential; when present it must be bound to the path conversation"),
        ("x-aionui-user-id" = Option<String>, Header, description = "Required with a runtime token"),
        ("x-aionui-conversation-id" = Option<String>, Header, description = "Required with a runtime token and must match the path")
    ),
    responses(
        (status = 200, description = "GEA Gateway Session", body = ApiResponse<GeaSessionResponse>),
        (status = 400, description = "Invalid consumer or conversation", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch or GEA access denied", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid or failed response", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = []),
        ("runtimeToken" = [])
    )
)]
async fn create_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    Json(request): Json<CreateGeaSessionRequest>,
) -> Result<Json<ApiResponse<GeaSessionResponse>>, GeaError> {
    enforce_runtime_conversation_scope(&headers, &conversation_id)?;
    let session = state
        .service
        .create_session(&user.id, &conversation_id, request)
        .await?;
    Ok(Json(ApiResponse::ok(session)))
}

#[utoipa::path(
    get,
    path = "/api/gea/conversations/{conversation_id}/tools",
    operation_id = "listGeaConversationTools",
    tag = "GEA tools",
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("x-aionui-runtime-token" = Option<String>, Header, description = "Conversation helper credential; when present it must be bound to the path conversation"),
        ("x-aionui-user-id" = Option<String>, Header, description = "Required with a runtime token"),
        ("x-aionui-conversation-id" = Option<String>, Header, description = "Required with a runtime token and must match the path")
    ),
    responses(
        (status = 200, description = "Tools authorized for the current GEA session", body = ApiResponse<Vec<GeaToolInfo>>),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch", body = ErrorResponse),
        (status = 409, description = "GEA conversation session is required", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid tool list", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = []),
        ("runtimeToken" = [])
    )
)]
async fn list_tools(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<GeaToolInfo>>>, GeaError> {
    enforce_runtime_conversation_scope(&headers, &conversation_id)?;
    let tools = state.service.list_tools(&user.id, &conversation_id).await?;
    Ok(Json(ApiResponse::ok(tools)))
}

#[utoipa::path(
    post,
    path = "/api/gea/mcp/test",
    operation_id = "testGeaMcpConnection",
    tag = "GEA tools",
    request_body(
        content = CreateGeaSessionRequest,
        example = json!({"consumerCode": "sales_forecast"})
    ),
    params(
        ("x-csrf-token" = Option<String>, Header, description = "Required outside local identity mode; must match the CSRF cookie")
    ),
    responses(
        (status = 200, description = "Tools discovered through a temporary GEA session", body = ApiResponse<Vec<GeaToolInfo>>),
        (status = 400, description = "Invalid MCP test payload", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credentials cannot use this trusted-client endpoint, or CSRF failed", body = ErrorResponse),
        (status = 502, description = "GEA session or tool discovery failed", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = [])
    )
)]
async fn test_mcp_connection(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    body: Result<Json<CreateGeaSessionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<GeaToolInfo>>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    let Json(request) = body.map_err(|_| GeaError::invalid_request("GEA MCP 测试参数无效"))?;
    let tools = state
        .service
        .test_mcp_connection(&user.id, request.consumer_code)
        .await?;
    Ok(Json(ApiResponse::ok(tools)))
}

#[utoipa::path(
    post,
    path = "/api/gea/conversations/{conversation_id}/tools/{tool_name}",
    operation_id = "callGeaConversationTool",
    tag = "GEA tools",
    request_body(
        content = GeaToolCallRequest,
        example = json!({"arguments": {"query": "example"}})
    ),
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("tool_name" = String, Path, description = "Authorized GEA tool name"),
        ("x-aionui-runtime-token" = Option<String>, Header, description = "Conversation helper credential; when present it must be bound to the path conversation"),
        ("x-aionui-user-id" = Option<String>, Header, description = "Required with a runtime token"),
        ("x-aionui-conversation-id" = Option<String>, Header, description = "Required with a runtime token and must match the path")
    ),
    responses(
        (status = 200, description = "GEA tool result and optional audit identifier", body = ApiResponse<GeaToolCallResponse>),
        (status = 400, description = "Tool arguments must be an object or null", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch", body = ErrorResponse),
        (status = 404, description = "Tool is not in the current authorized list", body = ErrorResponse),
        (status = 409, description = "GEA conversation session is required", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid or failed response", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = []),
        ("runtimeToken" = [])
    )
)]
async fn call_tool(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path((conversation_id, tool_name)): Path<(String, String)>,
    Json(request): Json<GeaToolCallRequest>,
) -> Result<Json<ApiResponse<GeaToolCallResponse>>, GeaError> {
    enforce_runtime_conversation_scope(&headers, &conversation_id)?;
    let result = state
        .service
        .call_tool(&user.id, &conversation_id, &tool_name, request.arguments)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeaResourceListQuery {
    cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/gea/conversations/{conversation_id}/resources",
    operation_id = "listGeaConversationResources",
    tag = "GEA tools",
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("cursor" = Option<String>, Query, description = "GEA MCP pagination cursor")
    ),
    responses(
        (status = 200, description = "Sanitized resources visible to the current GEA session", body = ApiResponse<GeaResourceList>),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch", body = ErrorResponse),
        (status = 409, description = "GEA conversation session is required", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid resource list", body = ErrorResponse)
    ),
    security(("runtimeToken" = []))
)]
async fn list_resources(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    Query(query): Query<GeaResourceListQuery>,
) -> Result<Json<ApiResponse<GeaResourceList>>, GeaError> {
    require_runtime_conversation_scope(&headers, &conversation_id)?;
    let resources = state
        .service
        .list_resources(&user.id, &conversation_id, query.cursor)
        .await?;
    Ok(Json(ApiResponse::ok(resources)))
}

#[utoipa::path(
    get,
    path = "/api/gea/conversations/{conversation_id}/resource-templates",
    operation_id = "listGeaConversationResourceTemplates",
    tag = "GEA tools",
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("cursor" = Option<String>, Query, description = "GEA MCP pagination cursor")
    ),
    responses(
        (status = 200, description = "Sanitized MCP resource templates", body = ApiResponse<GeaResourceTemplateList>),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch", body = ErrorResponse),
        (status = 409, description = "GEA conversation session is required", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid resource template list", body = ErrorResponse)
    ),
    security(("runtimeToken" = []))
)]
async fn list_resource_templates(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    Query(query): Query<GeaResourceListQuery>,
) -> Result<Json<ApiResponse<GeaResourceTemplateList>>, GeaError> {
    require_runtime_conversation_scope(&headers, &conversation_id)?;
    let templates = state
        .service
        .list_resource_templates(&user.id, &conversation_id, query.cursor)
        .await?;
    Ok(Json(ApiResponse::ok(templates)))
}

#[utoipa::path(
    post,
    path = "/api/gea/conversations/{conversation_id}/resources/read",
    operation_id = "readGeaConversationResource",
    tag = "GEA tools",
    request_body = ReadGeaResourceRequest,
    params(("conversation_id" = String, Path, description = "AionCore conversation identifier")),
    responses(
        (status = 200, description = "Integrity-checked UTF-8 resource contents", body = ApiResponse<GeaResourceContents>),
        (status = 400, description = "Invalid Resource URI", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch", body = ErrorResponse),
        (status = 404, description = "Resource is not bound to the current GEA session", body = ErrorResponse),
        (status = 502, description = "Resource integrity validation or upstream read failed", body = ErrorResponse)
    ),
    security(("runtimeToken" = []))
)]
async fn read_resource(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    Json(request): Json<ReadGeaResourceRequest>,
) -> Result<Json<ApiResponse<GeaResourceContents>>, GeaError> {
    require_runtime_conversation_scope(&headers, &conversation_id)?;
    let content = state
        .service
        .read_resource(&user.id, &conversation_id, &request.uri)
        .await?;
    Ok(Json(ApiResponse::ok(content)))
}

#[utoipa::path(
    get,
    path = "/api/gea/conversations/{conversation_id}/interaction-requests",
    operation_id = "listGeaConversationInteractionRequests",
    tag = "InteractionRequest",
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("x-aionui-runtime-token" = Option<String>, Header, description = "Conversation helper credential; when present it must be bound to the path conversation"),
        ("x-aionui-user-id" = Option<String>, Header, description = "Required with a runtime token"),
        ("x-aionui-conversation-id" = Option<String>, Header, description = "Required with a runtime token and must match the path")
    ),
    responses(
        (status = 200, description = "GEA-owned snapshot for one conversation", body = ApiResponse<GeaInteractionRequestSnapshot>),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch", body = ErrorResponse),
        (status = 409, description = "GEA conversation session is required", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid snapshot", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = []),
        ("runtimeToken" = [])
    )
)]
async fn list_interaction_requests(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<Json<ApiResponse<GeaInteractionRequestSnapshot>>, GeaError> {
    enforce_runtime_conversation_scope(&headers, &conversation_id)?;
    let snapshot = state
        .service
        .list_interaction_requests(&user.id, &conversation_id)
        .await?;
    Ok(Json(ApiResponse::ok(snapshot)))
}

#[utoipa::path(
    post,
    path = "/api/gea/conversations/{conversation_id}/interaction-requests/{request_id}/actions",
    operation_id = "actOnGeaConversationInteractionRequest",
    tag = "InteractionRequest",
    request_body(
        content = GeaInteractionRequestActionCommand,
        example = json!({
            "expectedVersion": "v1",
            "idempotencyKey": "example-command-1",
            "actionId": "answer",
            "payload": {"answers": [{"questionIndex": 0, "selectedLabels": ["Example"]}]}
        })
    ),
    params(
        ("conversation_id" = String, Path, description = "AionCore conversation identifier"),
        ("request_id" = String, Path, description = "GEA InteractionRequest identifier"),
        ("x-aionui-runtime-token" = Option<String>, Header, description = "Conversation helper credential; when present it must be bound to the path conversation"),
        ("x-aionui-user-id" = Option<String>, Header, description = "Required with a runtime token"),
        ("x-aionui-conversation-id" = Option<String>, Header, description = "Required with a runtime token and must match the path")
    ),
    responses(
        (status = 200, description = "GEA action receipt", body = ApiResponse<GeaInteractionRequestReceipt>),
        (status = 400, description = "Invalid action command", body = ErrorResponse),
        (status = 401, description = "AionCore or GEA authentication required", body = ErrorResponse),
        (status = 403, description = "Runtime credential conversation mismatch or action forbidden", body = ErrorResponse),
        (status = 404, description = "InteractionRequest not found", body = ErrorResponse),
        (status = 409, description = "Version or state conflict", body = ErrorResponse),
        (status = 502, description = "GEA returned an invalid or failed response", body = ErrorResponse)
    ),
    security(
        ("bearerAuth" = []),
        ("sessionCookie" = []),
        ("runtimeToken" = [])
    )
)]
async fn act_on_interaction_request(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path((conversation_id, request_id)): Path<(String, String)>,
    body: Result<Json<GeaInteractionRequestActionCommand>, JsonRejection>,
) -> Result<Json<ApiResponse<GeaInteractionRequestReceipt>>, GeaError> {
    enforce_runtime_conversation_scope(&headers, &conversation_id)?;
    let Json(command) = body.map_err(|_| GeaError::invalid_request("Interaction Request 动作参数无效"))?;
    let receipt = state
        .service
        .act_on_interaction_request(&user.id, &conversation_id, &request_id, command)
        .await?;
    Ok(Json(ApiResponse::ok(receipt)))
}

#[cfg(debug_assertions)]
struct SecuritySchemes;

#[cfg(debug_assertions)]
impl utoipa::Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Components::new);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .build(),
            ),
        );
        components.add_security_scheme(
            "sessionCookie",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("aionui-session"))),
        );
        components.add_security_scheme(
            "runtimeToken",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-aionui-runtime-token"))),
        );
    }
}

#[cfg(debug_assertions)]
#[derive(OpenApi)]
#[openapi(
    info(
        title = "AionCore GEA API",
        description = "Current AionCore interfaces used by AionUi and the conversation runtime. This document describes existing behavior and does not redefine the business contract."
    ),
    paths(
        auth_status,
        set_auth_session,
        clear_auth_session,
        create_session,
        list_tools,
        call_tool,
        list_resources,
        list_resource_templates,
        read_resource,
        test_mcp_connection,
        list_all_interaction_requests,
        act_on_global_interaction_request,
        list_notifications,
        get_notification,
        mark_notification_read,
        dismiss_notification,
        list_interaction_requests,
        act_on_interaction_request,
        sync_client_resources
    ),
    components(schemas(
        InteractionRequestChangedPayload,
        WebSocketMessage<InteractionRequestChangedPayload>,
        NotificationChangedPayload,
        WebSocketMessage<NotificationChangedPayload>,
        GeaNotification,
        GeaNotificationReceipt,
        GeaNotificationSnapshot,
        NotificationTarget
    )),
    modifiers(&SecuritySchemes),
    tags(
        (name = "GEA session", description = "GEA credential status and conversation Gateway Session"),
        (name = "GEA tools", description = "GEA MCP tool discovery, connection test, and invocation"),
        (name = "InteractionRequest", description = "GEA-owned requests and AionCore's recoverable user projection"),
        (name = "Notification", description = "GEA-owned user notifications and AionCore's tenant-scoped recoverable projection"),
        (name = "Client resources", description = "GEA Resource Catalog synchronization; current implementation materializes skills only")
    )
)]
struct GeaApiDoc;

fn reject_runtime_auth_session_access(headers: &HeaderMap) -> Result<(), GeaError> {
    if headers.contains_key(RUNTIME_TOKEN_HEADER) {
        return Err(GeaError::new(
            StatusCode::FORBIDDEN,
            "GEA_AUTH_SESSION_TRUSTED_CLIENT_REQUIRED",
            "GEA 登录态只能由受信客户端管理",
        ));
    }
    Ok(())
}

fn enforce_runtime_conversation_scope(headers: &HeaderMap, path_conversation_id: &str) -> Result<(), GeaError> {
    if !headers.contains_key(RUNTIME_TOKEN_HEADER) {
        return Ok(());
    }
    let header_conversation_id = headers
        .get(RUNTIME_CONVERSATION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if header_conversation_id != Some(path_conversation_id) {
        return Err(GeaError::new(
            StatusCode::FORBIDDEN,
            "GEA_CONVERSATION_SCOPE_MISMATCH",
            "运行时凭证与 GEA 对话不匹配",
        ));
    }
    Ok(())
}

fn require_runtime_conversation_scope(headers: &HeaderMap, path_conversation_id: &str) -> Result<(), GeaError> {
    if !headers.contains_key(RUNTIME_TOKEN_HEADER) {
        return Err(GeaError::new(
            StatusCode::FORBIDDEN,
            "GEA_RESOURCE_RUNTIME_REQUIRED",
            "GEA Resource 正文只能由受信对话运行时读取",
        ));
    }
    enforce_runtime_conversation_scope(headers, path_conversation_id)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::http::{HeaderMap, HeaderValue};
    use serde_json::Value;
    use utoipa::OpenApi;

    use super::{
        GeaApiDoc, enforce_runtime_conversation_scope, gea_swagger_config, reject_runtime_auth_session_access,
        require_runtime_conversation_scope,
    };
    use aionui_auth::{RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER};

    fn openapi_value() -> Value {
        serde_json::to_value(GeaApiDoc::openapi()).unwrap()
    }

    fn assert_schema_properties(document: &Value, name: &str, expected: &[&str]) {
        let schema = &document["components"]["schemas"][name];
        let actual = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("OpenAPI schema {name} must be an object"))
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(actual, expected, "OpenAPI schema properties drifted for {name}");
    }

    fn assert_schema_enum(document: &Value, name: &str, expected: &[&str]) {
        let actual = document["components"]["schemas"][name]["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("OpenAPI schema {name} must expose enum values"))
            .iter()
            .map(|value| value.as_str().expect("enum values must be strings"))
            .collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(actual, expected, "OpenAPI enum values drifted for {name}");
    }

    #[test]
    fn trusted_client_requests_do_not_require_runtime_conversation_headers() {
        assert!(enforce_runtime_conversation_scope(&HeaderMap::new(), "conversation-1").is_ok());
        assert!(reject_runtime_auth_session_access(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn runtime_requests_are_bound_to_the_path_conversation() {
        let mut headers = HeaderMap::new();
        headers.insert(RUNTIME_TOKEN_HEADER, HeaderValue::from_static("runtime-token"));
        headers.insert(
            RUNTIME_CONVERSATION_ID_HEADER,
            HeaderValue::from_static("conversation-1"),
        );

        assert!(enforce_runtime_conversation_scope(&headers, "conversation-1").is_ok());
        let error = enforce_runtime_conversation_scope(&headers, "conversation-2").unwrap_err();
        assert_eq!(error.body.code, "GEA_CONVERSATION_SCOPE_MISMATCH");
    }

    #[test]
    fn runtime_requests_cannot_read_or_replace_desktop_gea_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(RUNTIME_TOKEN_HEADER, HeaderValue::from_static("runtime-token"));

        let error = reject_runtime_auth_session_access(&headers).unwrap_err();
        assert_eq!(error.body.code, "GEA_AUTH_SESSION_TRUSTED_CLIENT_REQUIRED");
    }

    #[test]
    fn resource_contents_require_a_conversation_runtime_credential() {
        let error = require_runtime_conversation_scope(&HeaderMap::new(), "conversation-1").unwrap_err();
        assert_eq!(error.body.code, "GEA_RESOURCE_RUNTIME_REQUIRED");
    }

    #[test]
    fn gea_openapi_contains_the_current_route_set_and_unique_operation_ids() {
        let value = openapi_value();
        let paths = value["paths"].as_object().unwrap();
        let expected = [
            "/api/gea/auth/session",
            "/api/gea/conversations/{conversation_id}/session",
            "/api/gea/conversations/{conversation_id}/tools",
            "/api/gea/conversations/{conversation_id}/tools/{tool_name}",
            "/api/gea/conversations/{conversation_id}/resources",
            "/api/gea/conversations/{conversation_id}/resource-templates",
            "/api/gea/conversations/{conversation_id}/resources/read",
            "/api/gea/mcp/test",
            "/api/gea/conversations/{conversation_id}/interaction-requests",
            "/api/gea/conversations/{conversation_id}/interaction-requests/{request_id}/actions",
            "/api/interaction-requests",
            "/api/interaction-requests/{request_id}/actions",
            "/api/notifications",
            "/api/notifications/{notification_id}",
            "/api/notifications/{notification_id}/read",
            "/api/notifications/{notification_id}/dismiss",
            "/api/client-resources/sync",
        ];
        assert_eq!(paths.len(), expected.len());
        for path in expected {
            assert!(paths.contains_key(path), "missing OpenAPI path: {path}");
        }

        let mut operation_ids = std::collections::HashSet::new();
        for item in paths.values().filter_map(serde_json::Value::as_object) {
            for operation in item.values().filter_map(serde_json::Value::as_object) {
                let operation_id = operation
                    .get("operationId")
                    .and_then(serde_json::Value::as_str)
                    .expect("every documented operation must have an operationId");
                assert!(
                    operation_ids.insert(operation_id),
                    "duplicate operationId: {operation_id}"
                );
            }
        }
        assert_eq!(operation_ids.len(), 19);
    }

    #[test]
    fn gea_openapi_pins_session_and_tool_schema_fields() {
        let document = openapi_value();
        assert_schema_properties(&document, "SetGeaAuthSessionRequest", &["accessToken", "tenantId"]);
        assert_schema_properties(
            &document,
            "GeaAuthSessionStatus",
            &["authenticated", "reauthRequired", "tenantId"],
        );
        assert_schema_properties(&document, "CreateGeaSessionRequest", &["consumerCode", "preparationId"]);
        assert_schema_properties(
            &document,
            "GeaSessionResponse",
            &[
                "consumerCode",
                "conversationId",
                "effectiveCapabilityCodes",
                "sessionId",
            ],
        );
        assert_schema_properties(
            &document,
            "GeaToolInfo",
            &["description", "inputSchema", "name", "sourceCode"],
        );
        assert_schema_properties(&document, "GeaToolCallRequest", &["arguments"]);
        assert_schema_properties(
            &document,
            "GeaToolCallResponse",
            &["auditId", "content", "isError", "result"],
        );
        assert_schema_properties(
            &document,
            "GeaResourceContents",
            &["expiresAt", "mimeType", "sha256", "text", "uri"],
        );
    }

    #[test]
    fn gea_openapi_pins_interaction_request_schema_fields() {
        let document = openapi_value();
        assert_schema_properties(
            &document,
            "InteractionRequestList",
            &[
                "failed_session_count",
                "failure_codes",
                "items",
                "revision",
                "sync_state",
            ],
        );
        assert_schema_properties(
            &document,
            "InteractionRequestActionCommand",
            &["action_id", "expected_version", "idempotency_key", "payload"],
        );
        assert_schema_properties(
            &document,
            "InteractionRequestReceipt",
            &[
                "receipt_id",
                "request_id",
                "request",
                "resolved_at",
                "resolved_by",
                "status",
                "turn_continuation",
                "version",
            ],
        );
        assert_schema_properties(&document, "GeaInteractionRequestSnapshot", &["items", "revision"]);
        assert_schema_properties(
            &document,
            "GeaInteractionRequestActionCommand",
            &["actionId", "expectedVersion", "idempotencyKey", "payload"],
        );
        assert_schema_properties(
            &document,
            "GeaInteractionRequestReceipt",
            &[
                "auditId",
                "receiptId",
                "request",
                "requestId",
                "resolvedAt",
                "resolvedBy",
                "status",
                "turnContinuation",
                "version",
            ],
        );
        assert_schema_properties(&document, "InteractionRequestChangedPayload", &["revision", "user_id"]);
    }

    #[test]
    fn gea_openapi_pins_notification_schema_fields_and_values() {
        let document = openapi_value();
        assert_schema_properties(
            &document,
            "NotificationList",
            &["failure_codes", "items", "last_synced_at", "revision", "sync_state"],
        );
        assert_schema_properties(
            &document,
            "GeaNotificationSnapshot",
            &["items", "nextCursor", "revision"],
        );
        assert_schema_properties(
            &document,
            "NotificationView",
            &[
                "body",
                "created_at",
                "dismissible",
                "expires_at",
                "id",
                "interaction_request_id",
                "kind",
                "severity",
                "source",
                "status",
                "summary",
                "target",
                "title",
                "version",
            ],
        );
        assert_schema_properties(
            &document,
            "NotificationActionCommand",
            &["expected_version", "idempotency_key"],
        );
        assert_schema_properties(
            &document,
            "NotificationReceipt",
            &["notification", "notification_id", "receipt_id", "status", "version"],
        );
        assert_schema_properties(
            &document,
            "NotificationChangedPayload",
            &["notification_id", "reason", "revision", "trace_id"],
        );
        assert_schema_enum(&document, "NotificationStatus", &["dismissed", "read", "unread"]);
        assert_schema_enum(
            &document,
            "NotificationSyncState",
            &["failed", "fresh", "idle", "partial", "stale", "syncing"],
        );
    }

    #[test]
    fn gea_openapi_pins_client_resource_schema_fields_and_values() {
        let document = openapi_value();
        assert_schema_properties(&document, "SyncGeaClientResourcesRequest", &["resources"]);
        assert_schema_enum(&document, "GeaClientResourceKind", &["assistants", "mcps", "skills"]);
        assert_schema_properties(
            &document,
            "GeaClientResourceSyncResult",
            &["changed", "failed", "revision", "skipped", "status"],
        );
        assert_schema_enum(
            &document,
            "GeaClientResourceSyncStatus",
            &["completed", "notAuthenticated", "partial", "unavailable"],
        );
    }

    #[test]
    fn swagger_ui_disables_requests_and_external_validation() {
        let value = serde_json::to_value(gea_swagger_config()).unwrap();
        assert_eq!(value["supportedSubmitMethods"], serde_json::json!([]));
        assert_eq!(value["validatorUrl"], "none");
    }
}
