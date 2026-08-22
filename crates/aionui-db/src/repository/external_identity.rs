use crate::DbError;
use crate::models::{ExternalIdentity, ExternalIdentityProvider};

#[derive(Debug, Clone)]
pub struct EnsureExternalIdentityParams<'a> {
    pub provider: ExternalIdentityProvider,
    pub issuer: &'a str,
    pub tenant_id: &'a str,
    pub subject: &'a str,
    pub user_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureExternalIdentityResult {
    pub identity: ExternalIdentity,
    pub created: bool,
}

#[async_trait::async_trait]
pub trait IExternalIdentityRepository: Send + Sync {
    /// Atomically creates the tuple mapping or returns the existing mapping.
    /// A tuple already owned by another Core User fails with `DbError::Conflict`.
    async fn ensure(&self, params: EnsureExternalIdentityParams<'_>) -> Result<EnsureExternalIdentityResult, DbError>;

    /// Resolves the exact external identity tuple without creating a session.
    async fn find(
        &self,
        provider: ExternalIdentityProvider,
        issuer: &str,
        tenant_id: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, DbError>;
}
