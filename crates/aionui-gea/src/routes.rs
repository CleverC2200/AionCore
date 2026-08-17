use aionui_api_types::{
    ApiResponse, CreateGeaSessionRequest, GeaAuthSessionStatus, GeaInteractionRequestActionCommand,
    GeaInteractionRequestReceipt, GeaInteractionRequestSnapshot, GeaSessionResponse, GeaToolCallRequest,
    GeaToolCallResponse, GeaToolInfo, SetGeaAuthSessionRequest,
};
use aionui_auth::{CurrentUser, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER};
use axum::Router;
use axum::extract::{Extension, Json, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

use crate::error::GeaError;
use crate::state::GeaRouterState;

pub fn gea_routes(state: GeaRouterState) -> Router {
    Router::new()
        .route(
            "/api/gea/auth/session",
            get(auth_status).put(set_auth_session).delete(clear_auth_session),
        )
        .route("/api/gea/conversations/{conversation_id}/session", post(create_session))
        .route("/api/gea/conversations/{conversation_id}/tools", get(list_tools))
        .route(
            "/api/gea/conversations/{conversation_id}/interaction-requests",
            get(list_interaction_requests),
        )
        .route(
            "/api/gea/conversations/{conversation_id}/interaction-requests/{request_id}/actions",
            post(act_on_interaction_request),
        )
        .route(
            "/api/gea/conversations/{conversation_id}/tools/{tool_name}",
            post(call_tool),
        )
        .with_state(state)
}

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

async fn auth_status(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<GeaAuthSessionStatus>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    Ok(Json(ApiResponse::ok(state.service.auth_status(&user.id).await)))
}

async fn clear_auth_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<()>>, GeaError> {
    reject_runtime_auth_session_access(&headers)?;
    state.service.clear_auth_session(&user.id).await;
    Ok(Json(ApiResponse::success()))
}

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

async fn act_on_interaction_request(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    Path((conversation_id, request_id)): Path<(String, String)>,
    Json(command): Json<GeaInteractionRequestActionCommand>,
) -> Result<Json<ApiResponse<GeaInteractionRequestReceipt>>, GeaError> {
    enforce_runtime_conversation_scope(&headers, &conversation_id)?;
    let receipt = state
        .service
        .act_on_interaction_request(&user.id, &conversation_id, &request_id, command)
        .await?;
    Ok(Json(ApiResponse::ok(receipt)))
}

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

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::{enforce_runtime_conversation_scope, reject_runtime_auth_session_access};
    use aionui_auth::{RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER};

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
}
