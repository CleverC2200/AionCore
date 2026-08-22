use std::sync::Arc;

use aionui_api_types::{
    EnsureExternalIdentityMappingRequest, EnsureExternalIdentityMappingResponse,
    ExternalIdentityProvider as ApiExternalIdentityProvider,
};
use aionui_db::{
    DbError, ExternalIdentityProvider, IExternalIdentityRepository, ProvisionExternalIdentityError,
    ProvisionExternalIdentityParams,
};

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

impl ExternalIdentityMappingService {
    pub fn new(identity_repo: Arc<dyn IExternalIdentityRepository>) -> Self {
        Self { identity_repo }
    }

    pub async fn ensure_mapping(
        &self,
        request: EnsureExternalIdentityMappingRequest,
    ) -> Result<EnsureExternalIdentityMappingResponse, ExternalIdentityMappingError> {
        validate_exact_component(&request.identity.issuer, MAX_ISSUER_LENGTH)?;
        validate_exact_component(&request.identity.tenant_id, MAX_TENANT_ID_LENGTH)?;
        validate_exact_component(&request.identity.subject, MAX_SUBJECT_LENGTH)?;

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
