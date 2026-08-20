use aionui_common::TimestampMs;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredInteractionRequest {
    pub request_id: String,
    pub conversation_id: String,
    pub version: String,
    pub status: String,
    pub active: bool,
    pub kind: String,
    pub title: String,
    pub summary: Option<String>,
    pub source_label: Option<String>,
    pub allowed_actions: String,
    pub expires_at: Option<String>,
    pub updated_at: String,
    pub presentation: String,
    pub turn_id: Option<String>,
    pub message_id: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredInteractionRequestReceipt {
    pub idempotency_key: String,
    pub expected_version: String,
    pub action_id: String,
    pub receipt: String,
    pub resume_claim_owner: Option<String>,
    pub resume_claimed_at: Option<TimestampMs>,
    pub resume_started_at: Option<TimestampMs>,
    pub resume_delivered_at: Option<TimestampMs>,
    pub finalized_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredUnfinalizedInteractionRequestReceipt {
    pub request_id: String,
    pub idempotency_key: String,
    pub receipt: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredGeaSessionBootstrap {
    pub conversation_id: String,
    pub consumer_code: String,
    pub preparation_id: Option<String>,
}
