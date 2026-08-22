use async_trait::async_trait;

use crate::DbError;
use crate::models::{StoredNotification, StoredNotificationReceipt, StoredNotificationScope};

#[derive(Debug, Clone)]
pub struct UpsertNotificationParams {
    pub notification_id: String,
    pub version: String,
    pub status: String,
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub summary: Option<String>,
    pub body: Option<String>,
    pub dismissible: bool,
    pub source: String,
    pub target: String,
    pub interaction_request_id: Option<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReplaceNotificationSnapshotParams {
    pub user_id: String,
    pub tenant_id: String,
    pub revision: String,
    pub items: Vec<UpsertNotificationParams>,
    pub synced_at: i64,
}

#[derive(Debug, Clone)]
pub struct StoreNotificationReceiptParams {
    pub user_id: String,
    pub tenant_id: String,
    pub notification_id: String,
    pub idempotency_key: String,
    pub expected_version: String,
    pub action: String,
    pub receipt: String,
    pub created_at: i64,
    pub version: String,
    pub status: String,
}

#[async_trait]
pub trait INotificationRepository: Send + Sync {
    async fn replace_snapshot(&self, params: &ReplaceNotificationSnapshotParams) -> Result<bool, DbError>;

    async fn scope(&self, user_id: &str, tenant_id: &str) -> Result<Option<StoredNotificationScope>, DbError>;

    async fn list(
        &self,
        user_id: &str,
        tenant_id: &str,
        status: Option<&str>,
    ) -> Result<Vec<StoredNotification>, DbError>;

    async fn find(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
    ) -> Result<Option<StoredNotification>, DbError>;

    async fn load_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<StoredNotificationReceipt>, DbError>;

    async fn load_equivalent_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        expected_version: &str,
        action: &str,
    ) -> Result<Option<StoredNotificationReceipt>, DbError>;

    async fn store_receipt_and_update(&self, params: &StoreNotificationReceiptParams) -> Result<(), DbError>;
}
