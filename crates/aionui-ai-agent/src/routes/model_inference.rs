#![allow(clippy::disallowed_types)]

use super::error_mapping::agent_error_to_api_error;
use crate::ProviderModelInference;
use aionui_api_types::{ApiResponse, ModelInferenceRequest, ModelInferenceResponse};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::{
    Router,
    extract::{Extension, Json, State, rejection::JsonRejection},
    routing::post,
};
use std::sync::Arc;

/// The caller applies authentication and CSRF middleware.
pub fn model_inference_routes(service: Arc<ProviderModelInference>) -> Router {
    Router::new()
        .route("/api/models/inference", post(infer))
        .with_state(service)
}

async fn infer(
    State(service): State<Arc<ProviderModelInference>>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ModelInferenceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ModelInferenceResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        service
            .infer_default(&user.id, request.question)
            .await
            .map_err(agent_error_to_api_error)?,
    )))
}
