use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct StoredNotification {
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

#[derive(Debug, Clone, FromRow)]
pub struct StoredNotificationScope {
    pub revision: String,
    pub last_synced_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct StoredNotificationReceipt {
    pub idempotency_key: String,
    pub expected_version: String,
    pub action: String,
    pub receipt: String,
}
