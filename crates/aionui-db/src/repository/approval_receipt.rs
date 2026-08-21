use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredApprovalActionReceipt {
    pub idempotency_key: String,
    pub action: String,
    pub payload: String,
    pub instance_code: String,
    pub task_id: String,
    pub receipt: Option<String>,
}

pub struct CreateApprovalActionIntentParams {
    pub idempotency_key: String,
    pub action: String,
    pub payload: String,
    pub instance_code: String,
    pub task_id: String,
}

#[async_trait::async_trait]
pub trait IApprovalReceiptRepository: Send + Sync {
    async fn load(&self, idempotency_key: &str) -> Result<Option<StoredApprovalActionReceipt>, DbError>;
    async fn create_intent(&self, params: &CreateApprovalActionIntentParams) -> Result<(), DbError>;
    async fn store_receipt(&self, idempotency_key: &str, receipt: &str) -> Result<(), DbError>;
    async fn delete_intent(&self, idempotency_key: &str) -> Result<(), DbError>;
}
