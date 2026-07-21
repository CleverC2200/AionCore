#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_db::{IUserRepository, UserStatus, UserType};

use crate::JwtService;
use crate::extract::extract_token_from_headers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIdentityMode {
    Local,
    UserSession,
    AionPro,
}

/// Authenticated user injected into request extensions by the auth middleware.
///
/// Route handlers extract this from `request.extensions()` to identify
/// the current user.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// User ID from the database.
    pub id: String,
    /// Username.
    pub username: String,
    /// Internal identity source for the current user.
    pub user_type: UserType,
    /// Current account status. Authenticated requests only receive active users.
    pub status: UserStatus,
}

impl CurrentUser {
    pub fn local_default() -> Self {
        Self {
            id: "system_default_user".to_string(),
            username: "system_default_user".to_string(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }
}

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub identity_mode: AuthIdentityMode,
}

/// Authentication middleware that verifies JWT tokens and injects `CurrentUser`.
///
/// Flow:
/// 1. Extract bearer token from `Authorization` header or `aionui-session` cookie
/// 2. Verify JWT signature, expiration, and blacklist
/// 3. Look up user in the database to ensure they still exist
/// 4. Insert [`CurrentUser`] into request extensions
///
/// Returns HTTP 401 for authentication failures.
///
/// Use with `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // In local mode, skip JWT verification and inject a fixed default user.
    if state.identity_mode == AuthIdentityMode::Local {
        request.extensions_mut().insert(CurrentUser::local_default());
        return Ok(next.run(request).await);
    }

    let token = extract_token_from_headers(request.headers())
        .ok_or_else(|| ApiError::Unauthorized("Authentication required".into()))?;

    let payload = state.jwt_service.verify(&token).map_err(|e| {
        tracing::debug!("Token verification failed: {e}");
        ApiError::Unauthorized("Invalid or expired token".into())
    })?;

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "auth middleware user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    });

    Ok(next.run(request).await)
}

/// Local-mode authentication middleware that skips JWT verification.
///
/// Injects a fixed `CurrentUser` with id and username `system_default_user`.
/// Used when the server runs as an embedded subprocess inside Electron.
pub async fn local_auth_middleware(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(CurrentUser::local_default());
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    async fn echo_user(request: Request<Body>) -> String {
        let user = request.extensions().get::<CurrentUser>().unwrap();
        format!("{}:{}", user.id, user.username)
    }

    #[tokio::test]
    async fn test_local_auth_middleware_injects_default_user() {
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn(local_auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "system_default_user:system_default_user"
        );
    }
}
