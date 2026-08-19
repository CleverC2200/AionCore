use aionui_api_types::ErrorResponse;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaErrorBody {
    pub code: String,
    pub message: String,
    pub category: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
#[error("{body_message}")]
pub struct GeaError {
    pub status: StatusCode,
    pub body: Box<GeaErrorBody>,
    body_message: String,
}

impl GeaError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        let message = message.into();
        let category = match status {
            StatusCode::UNAUTHORIZED => "AUTHENTICATION",
            StatusCode::FORBIDDEN => "AUTHORIZATION",
            StatusCode::CONFLICT => "SESSION",
            StatusCode::TOO_MANY_REQUESTS => "RATE_LIMIT",
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT => "UPSTREAM",
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY => "VALIDATION",
            _ => "INTERNAL",
        };
        Self {
            status,
            body: Box::new(GeaErrorBody {
                code: code.into(),
                message: message.clone(),
                category: category.to_owned(),
                retryable: matches!(
                    status,
                    StatusCode::TOO_MANY_REQUESTS
                        | StatusCode::BAD_GATEWAY
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::GATEWAY_TIMEOUT
                ),
                retry_after_ms: None,
                request_id: None,
                trace_id: None,
                audit_id: None,
                details: None,
            }),
            body_message: message,
        }
    }

    pub fn unauthenticated() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "GEA_AUTH_REQUIRED", "请先登录 GEA")
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "GEA_INVALID_REQUEST", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "GEA_INTERACTION_REQUEST_NOT_FOUND", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "GEA_INTERACTION_REQUEST_STORAGE_ERROR",
            message,
        )
    }

    pub fn server_error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, message)
    }

    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn bad_gateway(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, code, message)
    }

    pub fn from_http_status(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        Self::new(status, code, message)
    }

    pub fn is_unauthorized(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED
    }

    pub fn session_required() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "GEA_SESSION_REQUIRED",
            "当前对话尚未建立 GEA 会话",
        )
    }

    pub fn tool_not_found(name: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "GEA_TOOL_NOT_FOUND",
            format!("工具 {name} 不在当前会话的授权列表中"),
        )
    }
}

impl IntoResponse for GeaError {
    fn into_response(self) -> Response {
        let details = serde_json::json!({
            "category": self.body.category,
            "retryable": self.body.retryable,
            "retryAfterMs": self.body.retry_after_ms,
            "requestId": self.body.request_id,
            "traceId": self.body.trace_id,
            "auditId": self.body.audit_id,
            "upstream": self.body.details,
        });
        let body = ErrorResponse::new_with_details(self.body.message, self.body.code, Some(details));
        (self.status, Json(body)).into_response()
    }
}
