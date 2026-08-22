use crate::DbError;
use crate::models::{ExternalIdentity, ExternalIdentityProvider, User};

#[derive(Debug, Clone)]
pub struct ProvisionExternalIdentityParams<'a> {
    pub provider: ExternalIdentityProvider,
    pub issuer: &'a str,
    pub tenant_id: &'a str,
    pub subject: &'a str,
}

#[derive(Debug, Clone)]
pub struct ProvisionExternalIdentityResult {
    pub identity: ExternalIdentity,
    pub user: User,
    pub created: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ProvisionExternalIdentityError {
    #[error("mapped Core user is disabled")]
    CoreUserDisabled,
    #[error("external identity conflicts with an incompatible Core user")]
    Conflict,
    #[error("external identity persistence failed")]
    Database(#[from] DbError),
}

#[async_trait::async_trait]
pub trait IExternalIdentityRepository: Send + Sync {
    /// Atomically provisions a passwordless generic external Core User and
    /// binds the exact tuple, or returns the existing compatible mapping.
    async fn provision(
        &self,
        params: ProvisionExternalIdentityParams<'_>,
    ) -> Result<ProvisionExternalIdentityResult, ProvisionExternalIdentityError>;

    /// Resolves the exact external identity tuple without creating a session.
    async fn find(
        &self,
        provider: ExternalIdentityProvider,
        issuer: &str,
        tenant_id: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, DbError>;
}
