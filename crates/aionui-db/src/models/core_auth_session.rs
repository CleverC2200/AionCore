use aionui_common::TimestampMs;

/// Durable server-side state for one renewable external Core session.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CoreAuthSession {
    pub sid: String,
    pub user_id: String,
    pub current_refresh_hash: String,
    pub previous_refresh_hash: Option<String>,
    pub last_rotation_key_hash: Option<String>,
    pub last_rotated_at: Option<TimestampMs>,
    pub session_generation: i64,
    pub rotation: i64,
    pub session_expires_at: TimestampMs,
    pub revoked_at: Option<TimestampMs>,
    pub revoke_reason: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
