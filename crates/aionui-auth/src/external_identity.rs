use std::sync::Arc;

use aionui_api_types::{
    EnsureExternalIdentityMappingRequest, EnsureExternalIdentityMappingResponse,
    ExternalIdentityProvider as ApiExternalIdentityProvider,
};
use aionui_db::{
    DbError, EnsureExternalIdentityParams, ExternalIdentityProvider, IExternalIdentityRepository, IUserRepository,
    UserStatus,
};

const MAX_CORE_USER_ID_LENGTH: usize = 255;
const MAX_ISSUER_LENGTH: usize = 512;
const MAX_TENANT_ID_LENGTH: usize = 255;
const MAX_SUBJECT_LENGTH: usize = 255;

#[derive(Debug, thiserror::Error)]
pub enum ExternalIdentityMappingError {
    #[error("invalid external identity mapping request")]
    InvalidInput,
    #[error("Core user not found")]
    CoreUserNotFound,
    #[error("Core user is disabled")]
    CoreUserDisabled,
    #[error("external identity is already mapped to another Core user")]
    Conflict,
    #[error("external identity persistence failed")]
    Database(#[source] DbError),
}

#[derive(Clone)]
pub struct ExternalIdentityMappingService {
    identity_repo: Arc<dyn IExternalIdentityRepository>,
    user_repo: Arc<dyn IUserRepository>,
}

impl ExternalIdentityMappingService {
    pub fn new(identity_repo: Arc<dyn IExternalIdentityRepository>, user_repo: Arc<dyn IUserRepository>) -> Self {
        Self {
            identity_repo,
            user_repo,
        }
    }

    pub async fn ensure_mapping(
        &self,
        request: EnsureExternalIdentityMappingRequest,
    ) -> Result<EnsureExternalIdentityMappingResponse, ExternalIdentityMappingError> {
        validate_exact_component(&request.core_user_id, MAX_CORE_USER_ID_LENGTH)?;
        validate_exact_component(&request.identity.issuer, MAX_ISSUER_LENGTH)?;
        validate_exact_component(&request.identity.tenant_id, MAX_TENANT_ID_LENGTH)?;
        validate_exact_component(&request.identity.subject, MAX_SUBJECT_LENGTH)?;

        let user = self
            .user_repo
            .find_by_id(&request.core_user_id)
            .await
            .map_err(ExternalIdentityMappingError::Database)?
            .ok_or(ExternalIdentityMappingError::CoreUserNotFound)?;
        if user.status == UserStatus::Disabled {
            return Err(ExternalIdentityMappingError::CoreUserDisabled);
        }

        let result = self
            .identity_repo
            .ensure(EnsureExternalIdentityParams {
                provider: map_provider(request.identity.provider),
                issuer: &request.identity.issuer,
                tenant_id: &request.identity.tenant_id,
                subject: &request.identity.subject,
                user_id: &request.core_user_id,
            })
            .await;

        let result = match result {
            Ok(result) => result,
            Err(DbError::NotFound(_)) => return Err(ExternalIdentityMappingError::CoreUserNotFound),
            Err(DbError::Conflict(_)) => {
                let user = self
                    .user_repo
                    .find_by_id(&request.core_user_id)
                    .await
                    .map_err(ExternalIdentityMappingError::Database)?;
                if user.is_some_and(|user| user.status == UserStatus::Disabled) {
                    return Err(ExternalIdentityMappingError::CoreUserDisabled);
                }
                return Err(ExternalIdentityMappingError::Conflict);
            }
            Err(error) => return Err(ExternalIdentityMappingError::Database(error)),
        };

        Ok(EnsureExternalIdentityMappingResponse {
            core_user_id: result.identity.user_id,
            created: result.created,
        })
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
