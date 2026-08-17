use aionui_api_types::{
    ApiResponse, CreateGeaSessionRequest, GeaAuthSessionStatus, GeaSessionResponse, GeaToolCallRequest,
    GeaToolCallResponse, GeaToolInfo, SetGeaAuthSessionRequest,
};
use aionui_auth::CurrentUser;
use axum::Router;
use axum::extract::{Extension, Json, Path, State};
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
            "/api/gea/conversations/{conversation_id}/tools/{tool_name}",
            post(call_tool),
        )
        .with_state(state)
}

async fn set_auth_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(request): Json<SetGeaAuthSessionRequest>,
) -> Result<Json<ApiResponse<GeaAuthSessionStatus>>, GeaError> {
    let status = state.service.set_auth_session(&user.id, request).await?;
    Ok(Json(ApiResponse::ok(status)))
}

async fn auth_status(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Json<ApiResponse<GeaAuthSessionStatus>> {
    Json(ApiResponse::ok(state.service.auth_status(&user.id).await))
}

async fn clear_auth_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Json<ApiResponse<()>> {
    state.service.clear_auth_session(&user.id).await;
    Json(ApiResponse::success())
}

async fn create_session(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<String>,
    Json(request): Json<CreateGeaSessionRequest>,
) -> Result<Json<ApiResponse<GeaSessionResponse>>, GeaError> {
    let session = state
        .service
        .create_session(&user.id, &conversation_id, request)
        .await?;
    Ok(Json(ApiResponse::ok(session)))
}

async fn list_tools(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<GeaToolInfo>>>, GeaError> {
    let tools = state.service.list_tools(&user.id, &conversation_id).await?;
    Ok(Json(ApiResponse::ok(tools)))
}

async fn call_tool(
    State(state): State<GeaRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation_id, tool_name)): Path<(String, String)>,
    Json(request): Json<GeaToolCallRequest>,
) -> Result<Json<ApiResponse<GeaToolCallResponse>>, GeaError> {
    let result = state
        .service
        .call_tool(&user.id, &conversation_id, &tool_name, request.arguments)
        .await?;
    Ok(Json(ApiResponse::ok(result)))
}
