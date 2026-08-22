use std::sync::Arc;

use aionui_api_types::{
    EnsureExternalIdentityMappingRequest, EnsureExternalIdentityMappingResponse,
    ExternalIdentityProvider as ApiExternalIdentityProvider, ExternalIdentityTuple, RevokeExternalSessionResponse,
};
use aionui_db::{
    DbError, ExternalIdentityProvider, IExternalIdentityRepository, IUserRepository, ProvisionExternalIdentityError,
    ProvisionExternalIdentityParams, UserStatus, UserType,
};

use crate::{RenewableSessionExchange, SessionLifecycle, SessionLifecycleError};

const MAX_ISSUER_LENGTH: usize = 512;
const MAX_TENANT_ID_LENGTH: usize = 255;
const MAX_SUBJECT_LENGTH: usize = 255;

#[derive(Debug, thiserror::Error)]
pub enum ExternalIdentityMappingError {
    #[error("invalid external identity mapping request")]
    InvalidInput,
    #[error("Core user is disabled")]
    CoreUserDisabled,
    #[error("external identity is mapped to an incompatible Core user")]
    Conflict,
    #[error("external identity persistence failed")]
    Database(#[source] DbError),
}

#[derive(Clone)]
pub struct ExternalIdentityMappingService {
    identity_repo: Arc<dyn IExternalIdentityRepository>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExternalIdentitySessionError {
    #[error("invalid external identity session request")]
    InvalidInput,
    #[error("external identity is not provisioned")]
    NotProvisioned,
    #[error("Core user is disabled")]
    CoreUserDisabled,
    #[error("external identity is mapped to an incompatible Core user")]
    Conflict,
    #[error("external identity session persistence failed")]
    Database(#[from] DbError),
    #[error("external session lifecycle failed")]
    Lifecycle(#[from] SessionLifecycleError),
}

#[derive(Clone)]
pub struct ExternalIdentitySessionService {
    identity_repo: Arc<dyn IExternalIdentityRepository>,
    user_repo: Arc<dyn IUserRepository>,
    session_lifecycle: Arc<SessionLifecycle>,
}

impl ExternalIdentityMappingService {
    pub fn new(identity_repo: Arc<dyn IExternalIdentityRepository>) -> Self {
        Self { identity_repo }
    }

    pub async fn ensure_mapping(
        &self,
        request: EnsureExternalIdentityMappingRequest,
    ) -> Result<EnsureExternalIdentityMappingResponse, ExternalIdentityMappingError> {
        validate_identity(&request.identity)?;

        let result = self
            .identity_repo
            .provision(ProvisionExternalIdentityParams {
                provider: map_provider(request.identity.provider),
                issuer: &request.identity.issuer,
                tenant_id: &request.identity.tenant_id,
                subject: &request.identity.subject,
            })
            .await
            .map_err(|error| match error {
                ProvisionExternalIdentityError::CoreUserDisabled => ExternalIdentityMappingError::CoreUserDisabled,
                ProvisionExternalIdentityError::Conflict => ExternalIdentityMappingError::Conflict,
                ProvisionExternalIdentityError::Database(error) => ExternalIdentityMappingError::Database(error),
            })?;

        Ok(EnsureExternalIdentityMappingResponse {
            core_user_id: result.user.id,
            created: result.created,
        })
    }
}

impl ExternalIdentitySessionService {
    pub fn new(
        identity_repo: Arc<dyn IExternalIdentityRepository>,
        user_repo: Arc<dyn IUserRepository>,
        session_lifecycle: Arc<SessionLifecycle>,
    ) -> Self {
        Self {
            identity_repo,
            user_repo,
            session_lifecycle,
        }
    }

    pub async fn create_session(
        &self,
        identity: ExternalIdentityTuple,
    ) -> Result<RenewableSessionExchange, ExternalIdentitySessionError> {
        let user = self.resolve_user(&identity).await?;
        self.user_repo.update_last_login(&user.id).await?;
        self.session_lifecycle.issue(user).await.map_err(Into::into)
    }

    pub async fn revoke_sessions(
        &self,
        identity: ExternalIdentityTuple,
    ) -> Result<RevokeExternalSessionResponse, ExternalIdentitySessionError> {
        let user = self.resolve_user(&identity).await?;
        let session_generation = self.session_lifecycle.revoke_user(&user.id).await?;
        Ok(RevokeExternalSessionResponse {
            user_id: user.id,
            session_generation,
        })
    }

    async fn resolve_user(
        &self,
        identity: &ExternalIdentityTuple,
    ) -> Result<aionui_db::models::User, ExternalIdentitySessionError> {
        validate_identity(identity).map_err(|_| ExternalIdentitySessionError::InvalidInput)?;
        let mapping = self
            .identity_repo
            .find(
                map_provider(identity.provider),
                &identity.issuer,
                &identity.tenant_id,
                &identity.subject,
            )
            .await?
            .ok_or(ExternalIdentitySessionError::NotProvisioned)?;
        let user = self
            .user_repo
            .find_by_id(&mapping.user_id)
            .await?
            .ok_or(ExternalIdentitySessionError::NotProvisioned)?;
        if user.user_type != UserType::External {
            return Err(ExternalIdentitySessionError::Conflict);
        }
        if user.status == UserStatus::Disabled {
            return Err(ExternalIdentitySessionError::CoreUserDisabled);
        }
        Ok(user)
    }
}

fn map_provider(provider: ApiExternalIdentityProvider) -> ExternalIdentityProvider {
    match provider {
        ApiExternalIdentityProvider::Lark => ExternalIdentityProvider::Lark,
    }
}

fn validate_exact_component(value: &str, max_length: usize) -> Result<(), ExternalIdentityMappingError> {
    if value.is_empty() || value.len() > max_length || value.trim() != value {
        return Err(ExternalIdentityMappingError::InvalidInput);
    }
    Ok(())
}

fn validate_identity(identity: &ExternalIdentityTuple) -> Result<(), ExternalIdentityMappingError> {
    validate_exact_component(&identity.issuer, MAX_ISSUER_LENGTH)?;
    validate_exact_component(&identity.tenant_id, MAX_TENANT_ID_LENGTH)?;
    validate_exact_component(&identity.subject, MAX_SUBJECT_LENGTH)
}
