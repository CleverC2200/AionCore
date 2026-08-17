#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::StatusCode;
use axum::routing::get;

use aionui_api_types::{
    ApiResponse, ManagedVoiceCapability, ManagedVoiceConfigurationResponse, ManagedVoiceHealthResponse,
    SetManagedVoiceConfigurationEnabledRequest, UpdateManagedVoiceConfigurationRequest, VoiceSessionCreateRequest,
    VoiceSessionCreateResponse, VoiceTurnRequest, VoiceTurnResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::error::VoiceError;
use crate::state::VoiceRouterState;

pub fn voice_capability_routes(state: VoiceRouterState) -> Router {
    Router::new()
        .route("/api/voice/capabilities", get(get_capabilities))
        .route("/api/voice/configurations", get(list_configurations))
        .with_state(state)
}

pub fn voice_configuration_action_routes(state: VoiceRouterState) -> Router {
    Router::new()
        .route("/api/voice/configurations", axum::routing::post(create_configuration))
        .route(
            "/api/voice/configurations/{configuration_id}",
            axum::routing::put(update_configuration).delete(delete_configuration),
        )
        .route(
            "/api/voice/configurations/{configuration_id}/enabled",
            axum::routing::put(set_configuration_enabled),
        )
        .route(
            "/api/voice/configurations/{configuration_id}/health",
            axum::routing::post(check_configuration_health),
        )
        .with_state(state)
}

pub fn voice_session_routes(state: VoiceRouterState) -> Router {
    Router::new()
        .route("/api/voice/sessions", axum::routing::post(create_session))
        .route(
            "/api/voice/sessions/{session_id}/start",
            axum::routing::post(start_session),
        )
        .route("/api/voice/sessions/{session_id}/turns", axum::routing::post(run_turn))
        .route("/api/voice/sessions/{session_id}", axum::routing::delete(stop_session))
        .with_state(state)
}

impl From<VoiceError> for ApiError {
    fn from(error: VoiceError) -> Self {
        match error {
            VoiceError::Disabled => ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "VOICE_NOT_CONFIGURED",
                "Managed voice is not configured.",
                None,
            ),
            VoiceError::AlreadyActive => ApiError::coded(
                StatusCode::CONFLICT,
                "VOICE_SESSION_ALREADY_ACTIVE",
                "An active voice session already exists.",
                None,
            ),
            VoiceError::SessionNotFound => ApiError::coded(
                StatusCode::NOT_FOUND,
                "VOICE_SESSION_NOT_FOUND",
                "Voice session not found.",
                None,
            ),
            VoiceError::ProviderUnavailable => ApiError::coded(
                StatusCode::BAD_GATEWAY,
                "VOICE_PROVIDER_UNAVAILABLE",
                "Voice provider is unavailable.",
                None,
            ),
            VoiceError::ConversationRequired => ApiError::coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "VOICE_CONVERSATION_REQUIRED",
                "Voice session must be bound to a conversation.",
                None,
            ),
            VoiceError::InvalidTranscript => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "VOICE_TRANSCRIPT_INVALID",
                "Voice transcript is invalid.",
                None,
            ),
            VoiceError::TurnBusy => ApiError::coded(
                StatusCode::CONFLICT,
                "VOICE_TURN_BUSY",
                "A voice turn is already running.",
                None,
            ),
            VoiceError::AgentUnavailable => ApiError::coded(
                StatusCode::BAD_GATEWAY,
                "VOICE_AGENT_UNAVAILABLE",
                "Conversation agent is unavailable.",
                None,
            ),
            VoiceError::InvalidConfiguration => ApiError::coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "VOICE_CONFIGURATION_INVALID",
                "Managed voice configuration is invalid.",
                None,
            ),
            VoiceError::ConfigurationUnavailable => ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "VOICE_CONFIGURATION_UNAVAILABLE",
                "Managed voice configuration is temporarily unavailable.",
                None,
            ),
            VoiceError::ConfigurationNotFound => ApiError::coded(
                StatusCode::NOT_FOUND,
                "VOICE_CONFIGURATION_NOT_FOUND",
                "Managed voice configuration not found.",
                None,
            ),
            VoiceError::ConfigurationManaged => ApiError::coded(
                StatusCode::FORBIDDEN,
                "VOICE_CONFIGURATION_MANAGED",
                "Managed voice configuration is read-only.",
                None,
            ),
        }
    }
}

async fn get_capabilities(
    State(state): State<VoiceRouterState>,
    Extension(_current_user): Extension<CurrentUser>,
) -> Json<ApiResponse<ManagedVoiceCapability>> {
    Json(ApiResponse::ok(state.service.capability(&_current_user.id).await))
}

async fn list_configurations(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<ManagedVoiceConfigurationResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.configurations(&current_user.id).await?,
    )))
}

async fn create_configuration(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<UpdateManagedVoiceConfigurationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ManagedVoiceConfigurationResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.create_configuration(&current_user.id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn update_configuration(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(configuration_id): Path<String>,
    body: Result<Json<UpdateManagedVoiceConfigurationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ManagedVoiceConfigurationResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .update_configuration(&current_user.id, &configuration_id, request)
            .await?,
    )))
}

async fn set_configuration_enabled(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(configuration_id): Path<String>,
    body: Result<Json<SetManagedVoiceConfigurationEnabledRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ManagedVoiceConfigurationResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_configuration_enabled(&current_user.id, &configuration_id, request.enabled)
            .await?,
    )))
}

async fn delete_configuration(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(configuration_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .delete_configuration(&current_user.id, &configuration_id)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn check_configuration_health(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(configuration_id): Path<String>,
) -> Result<Json<ApiResponse<ManagedVoiceHealthResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .configuration_health(&current_user.id, &configuration_id)
            .await?,
    )))
}

async fn create_session(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<VoiceSessionCreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<VoiceSessionCreateResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.create_session(&current_user.id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn stop_session(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(session_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.stop_session(&current_user.id, &session_id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn start_session(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(session_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.start_session(&current_user.id, &session_id).await?;
    Ok(Json(ApiResponse::success()))
}

async fn run_turn(
    State(state): State<VoiceRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(session_id): Path<String>,
    body: Result<Json<VoiceTurnRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<VoiceTurnResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.run_turn(&current_user.id, &session_id, request).await?;
    Ok(Json(ApiResponse::ok(response)))
}
