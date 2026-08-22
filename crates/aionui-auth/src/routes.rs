#![allow(clippy::disallowed_types)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::{
    ApiResponse, AuthStatusResponse, ChangePasswordRequest, EnsureExternalIdentityMappingRequest,
    EnsureExternalIdentityMappingResponse, EnsureExternalSessionRequest, EnsureExternalUserRequest,
    EnsureExternalUserResponse, LoginRequest, LoginResponse, PublicUser, QrLoginRequest, RefreshResponse,
    RefreshTokenRequest, RevokeExternalSessionRequest, RevokeExternalSessionResponse, UserInfoResponse,
    WebuiChangePasswordRequest, WebuiChangeUsernameRequest, WebuiChangeUsernameResponse, WebuiGenerateQrTokenResponse,
    WebuiResetPasswordResponse, WsTokenResponse,
};
use aionui_common::ApiError;
use aionui_common::constants::REFRESH_COOKIE_NAME;
use aionui_db::{DbError, IExternalIdentityRepository, IUserRepository, UserStatus, UserType, models::User};

use crate::error::AuthError;
use crate::external_identity::{
    ExternalIdentityMappingError, ExternalIdentityMappingService, ExternalIdentitySessionError,
    ExternalIdentitySessionService,
};
use crate::extract::{extract_cookie_value, extract_token_from_headers};
use crate::middleware::{AuthIdentityMode, AuthState, CurrentUser, auth_middleware};
use crate::password::{dummy_password_hash, generate_password, hash_password, verify_password_timed};
use crate::qr_token::QrTokenStore;
use crate::rate_limit::{
    RateLimiter, api_rate_limit_middleware, auth_rate_limit_middleware, authenticated_action_rate_limit_middleware,
};
use crate::service::{AuthProvisionService, ProvisionError};
use crate::validation::{validate_password, validate_username};
use crate::{CookieConfig, JwtService, SessionLifecycle, SessionLifecycleError};

const BOOTSTRAP_SECRET_HEADER: &str = "x-aioncore-bootstrap-secret";
const REFRESH_IDEMPOTENCY_HEADER: &str = "x-aioncore-refresh-idempotency-key";

pub type SessionRevokedHook = dyn Fn(&str) + Send + Sync;
pub type SessionRefreshedHook = dyn Fn(&str, &str, i64) + Send + Sync;
pub type MatchingSessionRevokedHook = dyn Fn(&str) + Send + Sync;

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::InvalidCredentials => ApiError::Unauthorized("Invalid username or password".into()),
            AuthError::WeakPassword(msg) => ApiError::BadRequest(msg),
            AuthError::InvalidUsername(msg) => ApiError::BadRequest(msg),
            AuthError::TokenExpired => ApiError::Unauthorized("Token expired".into()),
            AuthError::TokenInvalid(msg) => ApiError::Unauthorized(msg),
            AuthError::TokenBlacklisted => ApiError::Unauthorized("Token has been revoked".into()),
            AuthError::RateLimited => ApiError::RateLimited,
            AuthError::HashError(msg) => ApiError::Internal(format!("Password hash error: {msg}")),
        }
    }
}

fn db_error_to_api_error(err: DbError) -> ApiError {
    match err {
        DbError::NotFound(msg) => ApiError::NotFound(msg),
        DbError::Conflict(msg) => ApiError::Conflict(msg),
        DbError::Query(e) => ApiError::Internal(format!("Database error: {e}")),
        DbError::Migration(e) => ApiError::Internal(format!("Migration error: {e}")),
        DbError::Init(msg) => ApiError::Internal(format!("Database init error: {msg}")),
    }
}

/// Shared state for all auth route handlers.
#[derive(Clone)]
pub struct AuthRouterState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub external_identity_repo: Arc<dyn IExternalIdentityRepository>,
    pub session_lifecycle: Arc<SessionLifecycle>,
    /// Optional on-disk adoption side-effect (AionUi → AionPro upgrade).
    pub fs_adopter: Option<Arc<dyn crate::service::SystemDefaultFilesystemAdopter>>,
    pub cookie_config: Arc<CookieConfig>,
    pub qr_token_store: Arc<QrTokenStore>,
    pub identity_mode: AuthIdentityMode,
    pub bootstrap_secret: Option<Arc<str>>,
    pub session_revoked_hook: Option<Arc<SessionRevokedHook>>,
    pub session_refreshed_hook: Option<Arc<SessionRefreshedHook>>,
    pub matching_session_revoked_hook: Option<Arc<MatchingSessionRevokedHook>>,
    pub local: bool,
    pub aionpro_mode: bool,
}

#[derive(Debug, Deserialize)]
struct CreateInternalUserRequest {
    username: String,
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct SetSystemUserCredentialsRequest {
    username: String,
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct UpdatePasswordHashRequest {
    password_hash: String,
}

#[derive(Debug, Deserialize)]
struct UpdateUsernameRequest {
    username: String,
}

#[derive(Debug, Deserialize)]
struct UpdateJwtSecretRequest {
    jwt_secret: String,
}

#[derive(Debug, Serialize)]
struct InternalUserResponse {
    id: String,
    user_type: UserType,
    external_user_id: Option<String>,
    username: Option<String>,
    email: Option<String>,
    avatar_path: Option<String>,
    status: UserStatus,
    session_generation: i64,
    created_at: i64,
    updated_at: i64,
    last_login: Option<i64>,
}

impl From<User> for InternalUserResponse {
    fn from(user: User) -> Self {
        Self {
            id: user.id,
            user_type: user.user_type,
            external_user_id: user.external_user_id,
            username: user.username,
            email: user.email,
            avatar_path: user.avatar_path,
            status: user.status,
            session_generation: user.session_generation,
            created_at: user.created_at,
            updated_at: user.updated_at,
            last_login: user.last_login,
        }
    }
}

fn ensure_local_mode(local: bool) -> Result<(), ApiError> {
    if local {
        return Ok(());
    }
    Err(ApiError::Forbidden(
        "This endpoint is only available in local mode".into(),
    ))
}

fn require_bootstrap_secret(headers: &HeaderMap, expected: Option<&str>) -> Result<(), ApiError> {
    let Some(expected) = expected else {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "BOOTSTRAP_SECRET_REQUIRED",
            "Bootstrap secret required.",
            None,
        ));
    };
    let Some(actual) = headers.get(BOOTSTRAP_SECRET_HEADER).and_then(|v| v.to_str().ok()) else {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "BOOTSTRAP_SECRET_REQUIRED",
            "Bootstrap secret required.",
            None,
        ));
    };
    if constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "INVALID_BOOTSTRAP_SECRET",
            "Invalid bootstrap secret.",
            None,
        ))
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for idx in 0..max_len {
        let l = left.get(idx).copied().unwrap_or(0);
        let r = right.get(idx).copied().unwrap_or(0);
        diff |= usize::from(l ^ r);
    }
    diff == 0
}

fn provision_error_to_api_error(err: ProvisionError) -> ApiError {
    match err {
        ProvisionError::UnsupportedUserType => ApiError::BadRequest("Unsupported external user type".into()),
        ProvisionError::UserDisabled => ApiError::coded(StatusCode::FORBIDDEN, "USER_DISABLED", "User disabled.", None),
        ProvisionError::UserNotProvisioned => ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ),
        ProvisionError::Db(DbError::Conflict(_)) => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_USER_CONFLICT",
            "External user conflict.",
            None,
        ),
        ProvisionError::Db(e) => db_error_to_api_error(e),
        ProvisionError::Token(e) => ApiError::Internal(format!("Token signing error: {e}")),
    }
}

fn external_identity_error_to_api_error(err: ExternalIdentityMappingError) -> ApiError {
    match err {
        ExternalIdentityMappingError::InvalidInput => ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_IDENTITY_INVALID",
            "External identity is invalid.",
            None,
        ),
        ExternalIdentityMappingError::CoreUserDisabled => {
            ApiError::coded(StatusCode::FORBIDDEN, "CORE_USER_DISABLED", "Core user disabled.", None)
        }
        ExternalIdentityMappingError::Conflict => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_IDENTITY_CONFLICT",
            "External identity conflict.",
            None,
        ),
        ExternalIdentityMappingError::Database(_) => {
            tracing::error!("external identity mapping persistence failed");
            ApiError::Internal("External identity mapping unavailable".to_owned())
        }
    }
}

fn external_identity_session_error_to_api_error(err: ExternalIdentitySessionError) -> ApiError {
    match err {
        ExternalIdentitySessionError::InvalidInput => ApiError::coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_IDENTITY_INVALID",
            "External identity is invalid.",
            None,
        ),
        ExternalIdentitySessionError::NotProvisioned => user_context_required(),
        ExternalIdentitySessionError::CoreUserDisabled => {
            ApiError::coded(StatusCode::FORBIDDEN, "CORE_USER_DISABLED", "Core user disabled.", None)
        }
        ExternalIdentitySessionError::Conflict => ApiError::coded(
            StatusCode::CONFLICT,
            "EXTERNAL_IDENTITY_CONFLICT",
            "External identity conflict.",
            None,
        ),
        ExternalIdentitySessionError::Database(_) => {
            tracing::error!("external identity session persistence failed");
            ApiError::Internal("External identity session unavailable".to_owned())
        }
        ExternalIdentitySessionError::Lifecycle(error) => session_lifecycle_error_to_api_error(error),
    }
}

pub(crate) fn session_lifecycle_error_to_api_error(err: SessionLifecycleError) -> ApiError {
    let coded = |status, code, message| ApiError::coded(status, code, message, None);
    match err {
        SessionLifecycleError::RefreshRequired => coded(
            StatusCode::UNAUTHORIZED,
            "EXTERNAL_SESSION_REFRESH_REQUIRED",
            "External session refresh credential required.",
        ),
        SessionLifecycleError::RefreshInvalid => coded(
            StatusCode::UNAUTHORIZED,
            "EXTERNAL_SESSION_REFRESH_INVALID",
            "External session refresh credential invalid.",
        ),
        SessionLifecycleError::IdempotencyKeyRequired => coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_SESSION_REFRESH_IDEMPOTENCY_REQUIRED",
            "External session refresh idempotency key required.",
        ),
        SessionLifecycleError::IdempotencyKeyInvalid => coded(
            StatusCode::BAD_REQUEST,
            "EXTERNAL_SESSION_REFRESH_IDEMPOTENCY_INVALID",
            "External session refresh idempotency key invalid.",
        ),
        SessionLifecycleError::RefreshReplayed => coded(
            StatusCode::UNAUTHORIZED,
            "EXTERNAL_SESSION_REFRESH_REPLAYED",
            "External session refresh credential replayed.",
        ),
        SessionLifecycleError::Expired => coded(
            StatusCode::UNAUTHORIZED,
            "EXTERNAL_SESSION_EXPIRED",
            "External session expired.",
        ),
        SessionLifecycleError::Revoked => coded(
            StatusCode::UNAUTHORIZED,
            "EXTERNAL_SESSION_REVOKED",
            "External session revoked.",
        ),
        SessionLifecycleError::UserDisabled => {
            coded(StatusCode::FORBIDDEN, "CORE_USER_DISABLED", "Core user disabled.")
        }
        SessionLifecycleError::GenerationMismatch => coded(
            StatusCode::UNAUTHORIZED,
            "EXTERNAL_SESSION_GENERATION_MISMATCH",
            "External session generation is stale.",
        ),
        SessionLifecycleError::Database(_) | SessionLifecycleError::Token(_) | SessionLifecycleError::Clock => {
            tracing::error!("external session lifecycle unavailable");
            coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "EXTERNAL_SESSION_UNAVAILABLE",
                "External session unavailable.",
            )
        }
    }
}

fn user_context_required() -> ApiError {
    ApiError::coded(
        StatusCode::UNAUTHORIZED,
        "USER_CONTEXT_REQUIRED",
        "User context required.",
        None,
    )
}

/// Build the auth router with all endpoints and middleware layers.
///
/// Returns a `Router` with these endpoints:
/// - `POST /login`
/// - `POST /logout`
/// - `GET /api/auth/status`
/// - `GET /api/auth/user`
/// - `POST /api/auth/change-password`
/// - `POST /api/auth/refresh`
/// - `GET /api/ws-token`
/// - `POST /api/auth/qr-login`
/// - `PUT /api/auth/internal/external-identities` (trusted bootstrap only)
/// - `POST /api/auth/internal/external-sessions` (trusted bootstrap only)
/// - `POST /api/auth/internal/external-sessions/revoke` (trusted bootstrap only)
/// - `GET /qr-login`
/// - `POST /api/webui/change-password` (local-only)
/// - `POST /api/webui/change-username` (local-only)
/// - `POST /api/webui/reset-password` (local-only)
/// - `POST /api/webui/generate-qr-token` (local-only)
pub fn auth_routes(state: AuthRouterState) -> Router {
    let auth_limiter = Arc::new(RateLimiter::auth());
    let api_limiter = Arc::new(RateLimiter::api());
    let action_limiter = Arc::new(RateLimiter::authenticated_action());

    // Start periodic cleanup for rate limiters
    let cleanup_interval = Duration::from_secs(60);
    auth_limiter.start_cleanup_task(cleanup_interval);
    api_limiter.start_cleanup_task(cleanup_interval);
    action_limiter.start_cleanup_task(cleanup_interval);

    let auth_state = AuthState {
        jwt_service: state.jwt_service.clone(),
        user_repo: state.user_repo.clone(),
        session_lifecycle: Some(state.session_lifecycle.clone()),
        identity_mode: if state.aionpro_mode {
            AuthIdentityMode::AionPro
        } else {
            AuthIdentityMode::UserSession
        },
        // Auth endpoints manage sessions themselves; the helper CLI never
        // calls them, so the runtime-token channel stays disabled here.
        runtime_token_verifier: None,
    };

    // Auth rate limited routes (login, qr-login)
    let auth_rate_limited = Router::new()
        .route("/login", post(login_handler))
        .route("/api/auth/qr-login", post(qr_login_handler))
        .route_layer(from_fn_with_state(auth_limiter, auth_rate_limit_middleware))
        .with_state(state.clone());

    // API rate limited public routes (no auth required)
    let api_public = Router::new()
        .route("/api/auth/status", get(status_handler))
        .route(
            "/api/auth/internal/external-users/{external_user_id}",
            put(ensure_external_user_handler),
        )
        .route(
            "/api/auth/internal/external-identities",
            put(ensure_external_identity_mapping_handler),
        )
        .route(
            "/api/auth/internal/external-sessions",
            post(create_external_session_handler),
        )
        .route(
            "/api/auth/internal/external-sessions/revoke",
            post(revoke_external_session_handler),
        )
        .route(
            "/api/auth/internal/external-sessions/refresh",
            post(refresh_external_session_handler),
        )
        .route(
            "/api/auth/internal/external-sessions/revoke-matching",
            post(revoke_matching_external_session_handler),
        )
        .route(
            "/api/auth/internal/users",
            get(list_internal_users_handler).post(create_internal_user_handler),
        )
        .route("/api/auth/internal/users/system", get(get_system_user_handler))
        .route(
            "/api/auth/internal/users/system/credentials",
            post(set_system_user_credentials_handler),
        )
        .route(
            "/api/auth/internal/users/by-username/{username}",
            get(find_user_by_username_handler),
        )
        .route("/api/auth/internal/users/{id}", get(find_user_by_id_handler))
        .route(
            "/api/auth/internal/users/{id}/password",
            post(update_user_password_hash_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/username",
            post(update_user_username_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/jwt-secret",
            post(update_user_jwt_secret_handler),
        )
        .route(
            "/api/auth/internal/users/{id}/last-login",
            post(update_user_last_login_handler),
        )
        // WebUI admin credential endpoints — local-only, enforced inside each handler.
        .route("/api/webui/change-password", post(webui_change_password_handler))
        .route("/api/webui/change-username", post(webui_change_username_handler))
        .route("/api/webui/reset-password", post(webui_reset_password_handler))
        .route("/api/webui/generate-qr-token", post(webui_generate_qr_token_handler))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // Authenticated routes: api limiter -> auth -> action limiter
    // route_layer order: last added = outermost (first to process)
    let authenticated = Router::new()
        .route("/logout", post(logout_handler))
        .route("/api/auth/user", get(user_handler))
        .route("/api/auth/change-password", post(change_password_handler))
        .route("/api/ws-token", get(ws_token_handler))
        .route_layer(from_fn_with_state(
            action_limiter.clone(),
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(auth_state, auth_middleware))
        .route_layer(from_fn_with_state(api_limiter.clone(), api_rate_limit_middleware))
        .with_state(state.clone());

    // API + action limited routes (token in body, no auth middleware)
    let api_action_limited = Router::new()
        .route("/api/auth/refresh", post(refresh_handler))
        .route_layer(from_fn_with_state(
            action_limiter,
            authenticated_action_rate_limit_middleware,
        ))
        .route_layer(from_fn_with_state(api_limiter, api_rate_limit_middleware))
        .with_state(state);

    // Static page (no middleware)
    let static_routes = Router::new().route("/qr-login", get(qr_login_page));

    Router::new()
        .merge(auth_rate_limited)
        .merge(api_public)
        .merge(authenticated)
        .merge(api_action_limited)
        .merge(static_routes)
}

// ---------------------------------------------------------------------------
// PUT /api/auth/internal/external-identities
// ---------------------------------------------------------------------------

async fn ensure_external_identity_mapping_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Result<Json<EnsureExternalIdentityMappingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<EnsureExternalIdentityMappingResponse>>, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(request) = body.map_err(ApiError::from)?;
    let service = ExternalIdentityMappingService::new(state.external_identity_repo);
    let response = service
        .ensure_mapping(request)
        .await
        .map_err(external_identity_error_to_api_error)?;
    tracing::info!(created = response.created, "external identity mapping ensured");
    Ok(Json(ApiResponse::ok(response)))
}

// ---------------------------------------------------------------------------
// PUT /api/auth/internal/external-users/{external_user_id}
// ---------------------------------------------------------------------------

async fn ensure_external_user_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    Path(external_user_id): Path<String>,
    body: Result<Json<EnsureExternalUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<EnsureExternalUserResponse>>, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(req) = body.map_err(ApiError::from)?;
    let mut service = AuthProvisionService::new(state.user_repo, state.jwt_service);
    if let Some(fs_adopter) = state.fs_adopter {
        service = service.with_filesystem_adopter(fs_adopter);
    }
    let response = service
        .ensure_external_user(&external_user_id, req)
        .await
        .map_err(provision_error_to_api_error)?;
    tracing::info!(
        user_id = %response.user_id,
        user_type = ?response.user_type,
        "external user provision succeeded"
    );
    Ok(Json(ApiResponse::ok(response)))
}

// ---------------------------------------------------------------------------
// POST /api/auth/internal/external-sessions
// ---------------------------------------------------------------------------

async fn create_external_session_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Result<Json<EnsureExternalSessionRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(req) = body.map_err(ApiError::from)?;
    match req {
        EnsureExternalSessionRequest::AionPro(subject) => {
            let exchange = AuthProvisionService::new(state.user_repo, state.jwt_service)
                .create_external_session(subject)
                .await
                .map_err(provision_error_to_api_error)?;
            tracing::info!(user_id = %exchange.response.user.id, "legacy external Core session exchange succeeded");
            let cookie = state.cookie_config.build_session_cookie(&exchange.token);
            Ok(([(header::SET_COOKIE, cookie)], Json(ApiResponse::ok(exchange.response))).into_response())
        }
        EnsureExternalSessionRequest::ExternalIdentity(subject) => {
            let exchange = ExternalIdentitySessionService::new(
                state.external_identity_repo,
                state.user_repo,
                state.session_lifecycle,
            )
            .create_session(subject.identity)
            .await
            .map_err(external_identity_session_error_to_api_error)?;
            tracing::info!(sid = %exchange.response.session.sid, "renewable external Core session issued");
            Ok(external_session_response(
                &state.cookie_config,
                exchange.access_token,
                exchange.refresh_credential,
                exchange.response.session.access_expires_at,
                exchange.response.session.refresh_expires_at,
                ApiResponse::ok(exchange.response),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/auth/internal/external-sessions/revoke
// ---------------------------------------------------------------------------

async fn revoke_external_session_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Result<Json<RevokeExternalSessionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<RevokeExternalSessionResponse>>, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    let Json(req) = body.map_err(ApiError::from)?;
    let response = match req {
        RevokeExternalSessionRequest::AionPro(subject) => AuthProvisionService::new(state.user_repo, state.jwt_service)
            .revoke_external_session(subject)
            .await
            .map_err(provision_error_to_api_error)?,
        RevokeExternalSessionRequest::ExternalIdentity(subject) => {
            ExternalIdentitySessionService::new(state.external_identity_repo, state.user_repo, state.session_lifecycle)
                .revoke_sessions(subject.identity)
                .await
                .map_err(external_identity_session_error_to_api_error)?
        }
    };
    tracing::info!(
        user_id = %response.user_id,
        session_generation = response.session_generation,
        "external core session revoked"
    );
    if let Some(hook) = &state.session_revoked_hook {
        hook(&response.user_id);
    }
    Ok(Json(ApiResponse::ok(response)))
}

async fn refresh_external_session_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    require_empty_body(&body)?;
    let refresh_credential = extract_cookie_value(&headers, REFRESH_COOKIE_NAME);
    let idempotency_key = headers
        .get(REFRESH_IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok());
    let exchange = state
        .session_lifecycle
        .refresh(refresh_credential.as_deref(), idempotency_key)
        .await
        .map_err(session_lifecycle_error_to_api_error)?;
    if let Some(hook) = &state.session_refreshed_hook {
        hook(
            &exchange.response.session.sid,
            &exchange.access_token,
            exchange.response.session.rotation,
        );
    }
    tracing::info!(
        sid = %exchange.response.session.sid,
        rotation = exchange.response.session.rotation,
        "renewable external Core session refreshed"
    );
    Ok(external_session_response(
        &state.cookie_config,
        exchange.access_token,
        exchange.refresh_credential,
        exchange.response.session.access_expires_at,
        exchange.response.session.refresh_expires_at,
        ApiResponse::ok(exchange.response),
    ))
}

async fn revoke_matching_external_session_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_bootstrap_secret(&headers, state.bootstrap_secret.as_deref().map(AsRef::as_ref))?;
    require_empty_body(&body)?;
    let refresh_credential = extract_cookie_value(&headers, REFRESH_COOKIE_NAME);
    let response = state
        .session_lifecycle
        .revoke_matching(refresh_credential.as_deref())
        .await
        .map_err(session_lifecycle_error_to_api_error)?;
    if let Some(hook) = &state.matching_session_revoked_hook {
        hook(&response.sid);
    }
    tracing::info!(sid = %response.sid, "renewable external Core session revoked");
    let mut response = Json(ApiResponse::ok(response)).into_response();
    append_set_cookie(&mut response, state.cookie_config.clear_session_cookie());
    append_set_cookie(&mut response, state.cookie_config.clear_external_refresh_cookie());
    Ok(response)
}

fn require_empty_body(body: &Bytes) -> Result<(), ApiError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(ApiError::BadRequest("Request body must be empty".to_owned()))
    }
}

fn external_session_response<T: Serialize>(
    cookie_config: &CookieConfig,
    access_token: String,
    refresh_credential: String,
    access_expires_at: u64,
    refresh_expires_at: u64,
    payload: ApiResponse<T>,
) -> Response {
    let now = unix_now_secs();
    let mut response = Json(payload).into_response();
    append_set_cookie(
        &mut response,
        cookie_config.build_external_access_cookie(&access_token, access_expires_at.saturating_sub(now).max(1)),
    );
    append_set_cookie(
        &mut response,
        cookie_config.build_external_refresh_cookie(&refresh_credential, refresh_expires_at.saturating_sub(now).max(1)),
    );
    response
}

fn append_set_cookie(response: &mut Response, value: String) {
    if let Ok(value) = value.parse() {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// POST /login
// ---------------------------------------------------------------------------

async fn login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    if state.aionpro_mode {
        return Err(user_context_required());
    }

    let Json(req) = body.map_err(ApiError::from)?;

    // Input length validation (per API spec)
    if req.username.len() > 32 {
        return Err(ApiError::BadRequest("Username must not exceed 32 characters".into()));
    }
    if req.password.len() > 128 {
        return Err(ApiError::BadRequest("Password must not exceed 128 characters".into()));
    }

    // Look up user; run dummy verify on miss to prevent timing attacks
    let user = state
        .user_repo
        .find_by_username(&req.username)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    let (found_user, password_valid) = match user {
        Some(u) if u.password_hash.as_deref().unwrap_or_default().trim().is_empty() => {
            // Seeded user with no password yet (first-run local mode).
            // Treat as invalid credentials; run dummy verify for timing symmetry
            // and to avoid bcrypt error on empty hash leaking as a 500.
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
        Some(u) => {
            let Some(password_hash) = u.password_hash.as_deref() else {
                let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
                return Err(ApiError::Unauthorized("Invalid username or password".into()));
            };
            let valid = verify_password_timed(&req.password, password_hash).await?;
            (Some(u), valid)
        }
        None => {
            // Prevent user enumeration via timing
            let _ = verify_password_timed(&req.password, dummy_password_hash()).await;
            (None, false)
        }
    };

    if !password_valid {
        return Err(ApiError::Unauthorized("Invalid username or password".into()));
    }

    let user = found_user.ok_or_else(|| ApiError::Unauthorized("Invalid username or password".into()))?;

    let token = state
        .jwt_service
        .sign_with_session_generation(
            &user.id,
            user.username.as_deref().unwrap_or("external_user"),
            user.session_generation,
        )
        .map_err(|e| ApiError::Internal(format!("Token signing error: {e}")))?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(
        PublicUser {
            id: user.id,
            username: user.username.unwrap_or_else(|| "external_user".to_string()),
        },
        token,
    );

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// POST /logout
// ---------------------------------------------------------------------------

async fn logout_handler(State(state): State<AuthRouterState>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(token) = extract_token_from_headers(&headers) {
        state.jwt_service.blacklist_token(&token);
    }

    let cookie = state.cookie_config.clear_session_cookie();
    let resp = ApiResponse::message("Logged out successfully");

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// GET /api/auth/status
// ---------------------------------------------------------------------------

async fn status_handler(
    State(state): State<AuthRouterState>,
    headers: HeaderMap,
) -> Result<Json<AuthStatusResponse>, ApiError> {
    let has_users = state
        .user_repo
        .has_users()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    let user_count = state
        .user_repo
        .count_users()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    // Check authentication without requiring it
    let is_authenticated = extract_token_from_headers(&headers)
        .and_then(|token| state.jwt_service.verify(&token).ok())
        .is_some();

    Ok(Json(AuthStatusResponse {
        success: true,
        needs_setup: !has_users,
        user_count: user_count as u64,
        is_authenticated,
    }))
}

// ---------------------------------------------------------------------------
// Local-only internal user routes
// ---------------------------------------------------------------------------

async fn list_internal_users_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Vec<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let users = state.user_repo.list_users().await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(
        users.into_iter().map(InternalUserResponse::from).collect(),
    )))
}

async fn get_system_user_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<Option<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state.user_repo.get_system_user().await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user.map(InternalUserResponse::from))))
}

async fn find_user_by_username_handler(
    State(state): State<AuthRouterState>,
    Path(username): Path<String>,
) -> Result<Json<ApiResponse<Option<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state
        .user_repo
        .find_by_username(&username)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user.map(InternalUserResponse::from))))
}

async fn find_user_by_id_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Option<InternalUserResponse>>>, ApiError> {
    ensure_local_mode(state.local)?;
    let user = state.user_repo.find_by_id(&id).await.map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(user.map(InternalUserResponse::from))))
}

async fn create_internal_user_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<CreateInternalUserRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<InternalUserResponse>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let user = state
        .user_repo
        .create_user(&req.username, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(InternalUserResponse::from(user))))
}

async fn set_system_user_credentials_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<SetSystemUserCredentialsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .set_system_user_credentials(&req.username, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_password_hash_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePasswordHashRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .update_password(&id, &req.password_hash)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_username_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .update_username(&id, &req.username)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_jwt_secret_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
    body: Result<Json<UpdateJwtSecretRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .user_repo
        .update_jwt_secret(&id, &req.jwt_secret)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn update_user_last_login_handler(
    State(state): State<AuthRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    state
        .user_repo
        .update_last_login(&id)
        .await
        .map_err(db_error_to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

// ---------------------------------------------------------------------------
// GET /api/auth/user
// ---------------------------------------------------------------------------

async fn user_handler(Extension(user): Extension<CurrentUser>) -> Json<UserInfoResponse> {
    Json(UserInfoResponse {
        success: true,
        user: PublicUser {
            id: user.id,
            username: user.username,
        },
    })
}

// ---------------------------------------------------------------------------
// POST /api/auth/change-password
// ---------------------------------------------------------------------------

async fn change_password_handler(
    State(state): State<AuthRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<ChangePasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    // Validate new password strength
    validate_password(&req.new_password)?;

    // Fetch user record
    let user = state
        .user_repo
        .find_by_id(&current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::NotFound("User not found".into()))?;

    // Verify current password
    let Some(password_hash) = user.password_hash.as_deref() else {
        return Err(ApiError::Unauthorized("Current password is incorrect".into()));
    };
    let valid = verify_password_timed(&req.current_password, password_hash).await?;
    if !valid {
        return Err(ApiError::Unauthorized("Current password is incorrect".into()));
    }

    // Hash new password on blocking thread
    let password = req.new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    // Persist new password hash
    state
        .user_repo
        .update_password(&current_user.id, &new_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    // Revoke durable refresh sessions and rotate both access and refresh
    // signing material under the lifecycle's issue/refresh write barrier.
    let new_secret = state
        .session_lifecycle
        .rotate_master_secret()
        .await
        .map_err(session_lifecycle_error_to_api_error)?;

    // Persist new secret to database
    state
        .user_repo
        .update_jwt_secret(&current_user.id, &new_secret)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::message("Password changed successfully")))
}

// ---------------------------------------------------------------------------
// POST /api/auth/refresh
// ---------------------------------------------------------------------------

async fn refresh_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<RefreshTokenRequest>, JsonRejection>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let payload = state
        .jwt_service
        .verify(&req.token)
        .map_err(|_| ApiError::Unauthorized("Invalid or expired token".into()))?;

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "refresh token user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.aionpro_mode && user.user_type != aionui_db::UserType::Aionpro {
        return Err(user_context_required());
    }

    if user.user_type == aionui_db::UserType::External
        || payload.token_kind.is_some()
        || payload.sid.is_some()
        || payload.session_rotation.is_some()
    {
        return Err(ApiError::Unauthorized(
            "Renewable external sessions must use the trusted refresh endpoint".into(),
        ));
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    let new_token = state
        .jwt_service
        .sign_with_session_generation(
            &user.id,
            user.username.as_deref().unwrap_or("external_user"),
            user.session_generation,
        )
        .map_err(|e| ApiError::Internal(format!("Token signing error: {e}")))?;

    Ok(Json(RefreshResponse {
        success: true,
        token: new_token,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/ws-token
// ---------------------------------------------------------------------------

async fn ws_token_handler(
    State(state): State<AuthRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    headers: HeaderMap,
) -> Result<Json<WsTokenResponse>, ApiError> {
    // Reuse the existing session token for WebSocket connections
    let token = extract_token_from_headers(&headers).ok_or_else(|| ApiError::Unauthorized("No token found".into()))?;

    // Ensure user still exists
    state
        .user_repo
        .find_by_id(&current_user.id)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::Unauthorized("User not found".into()))?;

    let payload = state
        .jwt_service
        .verify(&token)
        .map_err(|_| ApiError::Unauthorized("Invalid or expired token".into()))?;
    let now = unix_now_secs();
    let expires_in = payload.exp.saturating_sub(now).saturating_mul(1000);

    Ok(Json(WsTokenResponse {
        success: true,
        ws_token: token,
        expires_in,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/auth/qr-login
// ---------------------------------------------------------------------------

async fn qr_login_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<QrLoginRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    if state.aionpro_mode {
        return Err(user_context_required());
    }

    let Json(req) = body.map_err(ApiError::from)?;

    // Validate and consume QR token (one-time use)
    state.qr_token_store.validate_and_consume(&req.qr_token)?;

    // Get primary WebUI user for QR login
    let user = state
        .user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::Internal("No primary user configured".into()))?;

    let token = state
        .jwt_service
        .sign_with_session_generation(
            &user.id,
            user.username.as_deref().unwrap_or("external_user"),
            user.session_generation,
        )
        .map_err(|e| ApiError::Internal(format!("Token signing error: {e}")))?;

    // Update last login (best-effort)
    if let Err(e) = state.user_repo.update_last_login(&user.id).await {
        tracing::warn!("Failed to update last login for {}: {e}", user.id);
    }

    let cookie = state.cookie_config.build_session_cookie(&token);
    let resp = LoginResponse::new(
        PublicUser {
            id: user.id,
            username: user.username.unwrap_or_else(|| "external_user".to_string()),
        },
        token,
    );

    Ok(([(header::SET_COOKIE, cookie)], Json(resp)).into_response())
}

// ---------------------------------------------------------------------------
// GET /qr-login (static HTML page)
// ---------------------------------------------------------------------------

async fn qr_login_page() -> Html<&'static str> {
    Html(QR_LOGIN_HTML)
}

const QR_LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>QR Login - AionUI</title>
<style>
  body { font-family: system-ui, sans-serif; display: flex; justify-content: center;
         align-items: center; min-height: 100vh; margin: 0; background: #f5f5f5; }
  .card { background: white; padding: 2rem; border-radius: 8px;
          box-shadow: 0 2px 8px rgba(0,0,0,0.1); text-align: center; max-width: 400px; }
  .status { margin-top: 1rem; color: #666; }
  .error { color: #d32f2f; }
  .success { color: #388e3c; }
</style>
</head>
<body>
<div class="card">
  <h1>AionUI</h1>
  <p id="status" class="status">Processing login...</p>
</div>
<script>
(function() {
  var el = document.getElementById('status');
  var params = new URLSearchParams(window.location.search);
  var token = params.get('token');
  if (!token) {
    el.textContent = 'Error: No token provided';
    el.className = 'status error';
    return;
  }
  fetch('/api/auth/qr-login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ qrToken: token })
  })
  .then(function(r) { return r.json(); })
  .then(function(data) {
    if (data.success) {
      el.textContent = 'Login successful! Redirecting...';
      el.className = 'status success';
      setTimeout(function() { window.location.href = '/'; }, 1000);
    } else {
      el.textContent = 'Login failed: ' + (data.error || 'Unknown error');
      el.className = 'status error';
    }
  })
  .catch(function(err) {
    el.textContent = 'Error: ' + err.message;
    el.className = 'status error';
  });
})();
</script>
</body>
</html>"#;

// ---------------------------------------------------------------------------
// WebUI admin credential endpoints (local-only)
// ---------------------------------------------------------------------------

/// Random password length for `/api/webui/reset-password`.
const RESET_PASSWORD_LEN: usize = 16;

/// Resolve the WebUI admin user, falling back to NotFound when absent.
async fn resolve_webui_admin(user_repo: &dyn IUserRepository) -> Result<User, ApiError> {
    user_repo
        .get_primary_webui_user()
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?
        .ok_or_else(|| ApiError::NotFound("No WebUI admin user configured".into()))
}

// ---------------------------------------------------------------------------
// POST /api/webui/change-password
// ---------------------------------------------------------------------------

async fn webui_change_password_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<WebuiChangePasswordRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;

    validate_password(&req.new_password)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    let password = req.new_password;
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    state
        .user_repo
        .update_password(&user.id, &new_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::message("Password changed successfully")))
}

// ---------------------------------------------------------------------------
// POST /api/webui/change-username
// ---------------------------------------------------------------------------

async fn webui_change_username_handler(
    State(state): State<AuthRouterState>,
    body: Result<Json<WebuiChangeUsernameRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<WebuiChangeUsernameResponse>>, ApiError> {
    ensure_local_mode(state.local)?;
    let Json(req) = body.map_err(ApiError::from)?;

    let trimmed = req.new_username.trim().to_owned();
    validate_username(&trimmed)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    if user.username.as_deref() != Some(trimmed.as_str()) {
        state
            .user_repo
            .update_username(&user.id, &trimmed)
            .await
            .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;
    }

    Ok(Json(ApiResponse::ok(WebuiChangeUsernameResponse { username: trimmed })))
}

// ---------------------------------------------------------------------------
// POST /api/webui/reset-password
// ---------------------------------------------------------------------------

async fn webui_reset_password_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<WebuiResetPasswordResponse>>, ApiError> {
    ensure_local_mode(state.local)?;

    let user = resolve_webui_admin(&*state.user_repo).await?;

    let new_password = generate_password(RESET_PASSWORD_LEN);
    let password_for_hash = new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password_for_hash))
        .await
        .map_err(|e| ApiError::Internal(format!("Task join error: {e}")))??;

    state
        .user_repo
        .update_password(&user.id, &new_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("Database error: {e}")))?;

    Ok(Json(ApiResponse::ok(WebuiResetPasswordResponse { new_password })))
}

// ---------------------------------------------------------------------------
// POST /api/webui/generate-qr-token
// ---------------------------------------------------------------------------

async fn webui_generate_qr_token_handler(
    State(state): State<AuthRouterState>,
) -> Result<Json<ApiResponse<WebuiGenerateQrTokenResponse>>, ApiError> {
    ensure_local_mode(state.local)?;

    let (token, expires_at_ms) = state.qr_token_store.generate_with_expiry();

    Ok(Json(ApiResponse::ok(WebuiGenerateQrTokenResponse {
        token,
        expires_at_ms,
    })))
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn invalid_credentials_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::InvalidCredentials);
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn weak_password_maps_to_bad_request() {
        let api_err = ApiError::from(AuthError::WeakPassword("too short".into()));
        assert_eq!(api_err.status_code(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn invalid_username_maps_to_bad_request() {
        let api_err = ApiError::from(AuthError::InvalidUsername("bad chars".into()));
        assert_eq!(api_err.status_code(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn token_expired_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::TokenExpired);
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn token_invalid_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::TokenInvalid("bad".into()));
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn token_blacklisted_maps_to_unauthorized() {
        let api_err = ApiError::from(AuthError::TokenBlacklisted);
        assert_eq!(api_err.status_code(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rate_limited_maps_to_rate_limited() {
        let api_err = ApiError::from(AuthError::RateLimited);
        assert_eq!(api_err.status_code(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn hash_error_maps_to_internal() {
        let api_err = ApiError::from(AuthError::HashError("failed".into()));
        assert_eq!(api_err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
