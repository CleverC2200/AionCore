use aionui_common::TimestampMs;

use crate::DbError;
use crate::models::{
    StoredGeaSessionBootstrap, StoredInteractionRequest, StoredInteractionRequestReceipt,
    StoredUnfinalizedInteractionRequestReceipt,
};

#[derive(Debug, Clone)]
pub struct UpsertInteractionRequestParams {
    pub user_id: String,
    pub request_id: String,
    pub conversation_id: String,
    pub version: String,
    pub status: String,
    pub kind: String,
    pub title: String,
    pub summary: Option<String>,
    pub source_label: Option<String>,
    pub allowed_actions: String,
    pub expires_at: Option<String>,
    pub updated_at: String,
    pub presentation: String,
    pub upstream_revision: String,
    pub turn_id: Option<String>,
    pub message_id: String,
    pub changed_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct StoreInteractionRequestReceiptParams {
    pub user_id: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub expected_version: String,
    pub action_id: String,
    pub receipt: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct UpsertGeaSessionBootstrapParams {
    pub user_id: String,
    pub conversation_id: String,
    pub consumer_code: String,
    pub preparation_id: Option<String>,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptResumeClaim {
    Acquired,
    Delivered,
    Unknown,
    Busy,
}

#[async_trait::async_trait]
pub trait IInteractionRequestRepository: Send + Sync {
    async fn upsert_session_bootstrap(&self, params: &UpsertGeaSessionBootstrapParams) -> Result<(), DbError>;
    async fn list_pending_session_bootstraps(&self, user_id: &str) -> Result<Vec<StoredGeaSessionBootstrap>, DbError>;
    async fn conversation_exists(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError>;
    async fn list_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<StoredInteractionRequest>, DbError>;
    async fn list_pending(&self, user_id: &str) -> Result<Vec<StoredInteractionRequest>, DbError>;
    async fn find(&self, user_id: &str, request_id: &str) -> Result<Option<StoredInteractionRequest>, DbError>;
    async fn upsert(&self, params: &UpsertInteractionRequestParams) -> Result<(), DbError>;
    async fn resolve_missing(
        &self,
        user_id: &str,
        conversation_id: &str,
        incoming_request_ids: &[String],
        changed_at: TimestampMs,
    ) -> Result<(), DbError>;
    async fn update_authoritative(
        &self,
        user_id: &str,
        request_id: &str,
        update: &UpsertInteractionRequestParams,
    ) -> Result<(), DbError>;
    async fn update_status(
        &self,
        user_id: &str,
        request_id: &str,
        status: &str,
        version: &str,
        changed_at: TimestampMs,
    ) -> Result<(), DbError>;
    async fn load_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<StoredInteractionRequestReceipt>, DbError>;
    async fn load_equivalent_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        expected_version: &str,
        action_id: &str,
    ) -> Result<Option<StoredInteractionRequestReceipt>, DbError>;
    async fn list_unfinalized_receipts(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredUnfinalizedInteractionRequestReceipt>, DbError>;
    async fn store_receipt(&self, params: &StoreInteractionRequestReceiptParams) -> Result<(), DbError>;
    async fn claim_receipt_resume(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
        claimed_at: TimestampMs,
        stale_before: TimestampMs,
    ) -> Result<ReceiptResumeClaim, DbError>;
    async fn mark_receipt_resume_started(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
        started_at: TimestampMs,
    ) -> Result<bool, DbError>;
    async fn mark_receipt_resume_delivered(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
        delivered_at: TimestampMs,
    ) -> Result<bool, DbError>;
    async fn mark_receipt_finalized(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        require_resume_delivered: bool,
        finalized_at: TimestampMs,
    ) -> Result<bool, DbError>;
}
