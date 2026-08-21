use sqlx::SqlitePool;

use crate::error::DbError;
use crate::repository::approval_receipt::{
    CreateApprovalActionIntentParams, IApprovalReceiptRepository, StoredApprovalActionReceipt,
};

#[derive(Clone, Debug)]
pub struct SqliteApprovalReceiptRepository {
    pool: SqlitePool,
}

impl SqliteApprovalReceiptRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IApprovalReceiptRepository for SqliteApprovalReceiptRepository {
    async fn load(&self, idempotency_key: &str) -> Result<Option<StoredApprovalActionReceipt>, DbError> {
        Ok(sqlx::query_as::<_, StoredApprovalActionReceipt>(
            "SELECT idempotency_key, action, payload, instance_code, task_id, receipt \
             FROM approval_action_receipts WHERE idempotency_key = ?",
        )
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn create_intent(&self, params: &CreateApprovalActionIntentParams) -> Result<(), DbError> {
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO approval_action_receipts \
             (idempotency_key, action, payload, instance_code, task_id, receipt, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(&params.idempotency_key)
        .bind(&params.action)
        .bind(&params.payload)
        .bind(&params.instance_code)
        .bind(&params.task_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn store_receipt(&self, idempotency_key: &str, receipt: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE approval_action_receipts SET receipt = ?, updated_at = ? WHERE idempotency_key = ?")
            .bind(receipt)
            .bind(aionui_common::now_ms())
            .bind(idempotency_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_intent(&self, idempotency_key: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM approval_action_receipts WHERE idempotency_key = ? AND receipt IS NULL")
            .bind(idempotency_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
