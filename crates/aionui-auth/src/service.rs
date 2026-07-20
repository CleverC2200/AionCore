use std::sync::Arc;

use aionui_api_types::{
    EnsureExternalSessionRequest, EnsureExternalSessionResponse, EnsureExternalUserRequest, EnsureExternalUserResponse,
    ExternalUserType, PublicUser,
};
use aionui_db::{ExternalUserProjection, IUserRepository, UserStatus, UserType, models::User};

use crate::JwtService;

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("unsupported external user type")]
    UnsupportedUserType,
    #[error("external user is disabled")]
    UserDisabled,
    #[error("external user is not provisioned")]
    UserNotProvisioned,
    #[error("database error: {0}")]
    Db(#[from] aionui_db::DbError),
    #[error("token signing failed: {0}")]
    Token(#[from] crate::AuthError),
}

#[derive(Clone)]
pub struct AuthProvisionService {
    user_repo: Arc<dyn IUserRepository>,
    jwt_service: Arc<JwtService>,
}

impl AuthProvisionService {
    pub fn new(user_repo: Arc<dyn IUserRepository>, jwt_service: Arc<JwtService>) -> Self {
        Self { user_repo, jwt_service }
    }

    pub async fn ensure_external_user(
        &self,
        external_user_id: &str,
        request: EnsureExternalUserRequest,
    ) -> Result<EnsureExternalUserResponse, ProvisionError> {
        let user_type = map_external_user_type(request.user_type)?;
        let user = self
            .user_repo
            .ensure_external_user(
                user_type,
                external_user_id,
                ExternalUserProjection {
                    username: request.username,
                    email: request.email,
                    avatar_path: request.avatar_path,
                },
            )
            .await?;

        if user.status == UserStatus::Disabled {
            return Err(ProvisionError::UserDisabled);
        }

        Ok(external_user_response(user, request.user_type))
    }

    pub async fn create_external_session(
        &self,
        request: EnsureExternalSessionRequest,
    ) -> Result<EnsureExternalSessionResponse, ProvisionError> {
        let user_type = map_external_user_type(request.user_type)?;
        let user = self
            .user_repo
            .find_by_external_user_id(user_type, &request.external_user_id)
            .await?
            .ok_or(ProvisionError::UserNotProvisioned)?;

        if user.status == UserStatus::Disabled {
            return Err(ProvisionError::UserDisabled);
        }

        let username = user.username.clone().unwrap_or_else(|| "external_user".to_string());
        let token = self.jwt_service.sign(&user.id, &username)?;
        self.user_repo.update_last_login(&user.id).await?;

        Ok(EnsureExternalSessionResponse {
            user: PublicUser { id: user.id, username },
            token,
            session_generation: user.session_generation,
        })
    }
}

fn map_external_user_type(user_type: ExternalUserType) -> Result<UserType, ProvisionError> {
    match user_type {
        ExternalUserType::Aionpro => Ok(UserType::Aionpro),
    }
}

fn external_user_response(user: User, user_type: ExternalUserType) -> EnsureExternalUserResponse {
    EnsureExternalUserResponse {
        user_id: user.id,
        user_type,
        external_user_id: user.external_user_id.unwrap_or_default(),
        session_generation: user.session_generation,
    }
}
