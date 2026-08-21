#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalUpstreamError {
    StaleTask,
    Permission,
    Other,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApprovalError {
    #[error("invalid approval request: {0}")]
    Invalid(String),
    #[error("approval access requires a trusted local client")]
    TrustedClientRequired,
    #[error("approval provider unavailable")]
    ProviderUnavailable,
    #[error("approval provider returned invalid data")]
    InvalidProviderResponse,
    #[error("approval provider rejected the request: {0:?}")]
    Upstream(ApprovalUpstreamError),
    #[error("idempotency key reused for a different approval action")]
    IdempotencyConflict,
    #[error("approval receipt storage unavailable")]
    StorageUnavailable,
}

impl ApprovalError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub fn upstream(code: Option<&str>) -> Self {
        Self::Upstream(match code {
            Some("1395001") => ApprovalUpstreamError::StaleTask,
            Some("99991663" | "99991668" | "99991672") => ApprovalUpstreamError::Permission,
            _ => ApprovalUpstreamError::Other,
        })
    }
}
