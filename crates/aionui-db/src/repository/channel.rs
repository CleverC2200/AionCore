use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{AssistantSessionRow, AssistantUserRow, ChannelConnectionRow, PairingCodeRow};

/// Data access abstraction for channel integration tables.
///
/// Covers four tables: `channel_connections`, `assistant_users`,
/// `assistant_sessions`, and `assistant_pairing_codes`.
///
/// Object-safe via `async_trait` to support `Arc<dyn IChannelRepository>`.
#[async_trait::async_trait]
pub trait IChannelRepository: Send + Sync {
    // ── Connection CRUD ──────────────────────────────────────────────

    /// Returns all channel connections for an owner.
    async fn get_all_connections(&self, owner_user_id: &str) -> Result<Vec<ChannelConnectionRow>, DbError>;

    /// Returns a single connection by connection id, or `None` if not found.
    async fn get_connection(&self, owner_user_id: &str, id: &str) -> Result<Option<ChannelConnectionRow>, DbError>;

    /// Returns the owner's connection for a plugin key, or `None`.
    ///
    /// Phase 1 guarantees at most one connection per (owner, plugin_key)
    /// (`idx_channel_connections_single_instance`), which is what makes this
    /// lookup well-defined; it is the bridge for callers that still address
    /// channels by platform.
    async fn get_connection_by_plugin_key(
        &self,
        owner_user_id: &str,
        plugin_key: &str,
    ) -> Result<Option<ChannelConnectionRow>, DbError>;

    /// Inserts a new connection or updates an existing one (by connection id).
    async fn upsert_connection(&self, owner_user_id: &str, row: &ChannelConnectionRow) -> Result<(), DbError>;

    /// Updates only the `status` / `last_connected` / `enabled` of a connection.
    async fn update_connection_status(
        &self,
        owner_user_id: &str,
        id: &str,
        params: &UpdateConnectionStatusParams,
    ) -> Result<(), DbError>;

    /// Deletes a connection by connection id. Returns `DbError::NotFound` if absent.
    async fn delete_connection(&self, owner_user_id: &str, id: &str) -> Result<(), DbError>;

    // ── User CRUD ────────────────────────────────────────────────────

    /// Returns all authorized users for an owner.
    async fn get_all_users(&self, owner_user_id: &str) -> Result<Vec<AssistantUserRow>, DbError>;

    /// Finds a user by platform identity. Returns `None` if not found.
    async fn get_user_by_platform(
        &self,
        owner_user_id: &str,
        platform_user_id: &str,
        platform_type: &str,
    ) -> Result<Option<AssistantUserRow>, DbError>;

    /// Creates a new authorized user record.
    async fn create_user(&self, owner_user_id: &str, row: &AssistantUserRow) -> Result<(), DbError>;

    /// Updates `last_active` timestamp for a user.
    async fn update_user_last_active(
        &self,
        owner_user_id: &str,
        id: &str,
        last_active: TimestampMs,
    ) -> Result<(), DbError>;

    /// Deletes a user by id. Returns `DbError::NotFound` if absent.
    /// Associated sessions are cascade-deleted by the database.
    async fn delete_user(&self, owner_user_id: &str, id: &str) -> Result<(), DbError>;

    // ── Session CRUD ─────────────────────────────────────────────────

    /// Returns all sessions for an owner.
    async fn get_all_sessions(&self, owner_user_id: &str) -> Result<Vec<AssistantSessionRow>, DbError>;

    /// Returns a single session by id.
    async fn get_session(&self, owner_user_id: &str, id: &str) -> Result<Option<AssistantSessionRow>, DbError>;

    /// Finds an existing session by user + chat, or creates a new one.
    /// If found, updates `last_activity` and returns the existing row.
    /// If not found, inserts `new_row` and returns it.
    async fn get_or_create_session(
        &self,
        owner_user_id: &str,
        channel_user_id: &str,
        chat_id: &str,
        new_row: &AssistantSessionRow,
    ) -> Result<AssistantSessionRow, DbError>;

    /// Updates `last_activity` timestamp for a session.
    async fn update_session_activity(
        &self,
        owner_user_id: &str,
        id: &str,
        last_activity: TimestampMs,
    ) -> Result<(), DbError>;

    /// Updates the `conversation_id` of a session.
    async fn update_session_conversation(
        &self,
        owner_user_id: &str,
        id: &str,
        conversation_id: &str,
    ) -> Result<(), DbError>;

    /// Updates the `agent_type` of a session.
    async fn update_session_agent_type(&self, owner_user_id: &str, id: &str, agent_type: &str) -> Result<(), DbError>;

    /// Deletes all sessions belonging to a user.
    async fn delete_sessions_by_user(&self, owner_user_id: &str, channel_user_id: &str) -> Result<(), DbError>;

    /// Deletes the session for a specific user + chat pair.
    async fn delete_session_by_user_chat(
        &self,
        owner_user_id: &str,
        channel_user_id: &str,
        chat_id: &str,
    ) -> Result<(), DbError>;

    // ── Pairing Codes ────────────────────────────────────────────────

    /// Creates a new pairing code record.
    async fn create_pairing(&self, owner_user_id: &str, row: &PairingCodeRow) -> Result<(), DbError>;

    /// Returns all pairing codes with status = 'pending'.
    async fn get_pending_pairings(&self, owner_user_id: &str) -> Result<Vec<PairingCodeRow>, DbError>;

    /// Retrieves a single pairing code, or `None` if not found.
    async fn get_pairing_by_code(&self, owner_user_id: &str, code: &str) -> Result<Option<PairingCodeRow>, DbError>;

    /// Updates the status of a pairing code.
    /// Returns `DbError::NotFound` if the code doesn't exist.
    async fn update_pairing_status(&self, owner_user_id: &str, code: &str, status: &str) -> Result<(), DbError>;

    /// Marks all expired-but-still-pending pairing codes as 'expired'.
    /// `now` is the current timestamp in milliseconds.
    async fn cleanup_expired_pairings(&self, owner_user_id: &str, now: TimestampMs) -> Result<u64, DbError>;
}

/// Parameters for updating connection runtime status.
#[derive(Debug, Clone, Default)]
pub struct UpdateConnectionStatusParams {
    pub status: Option<String>,
    pub last_connected: Option<TimestampMs>,
    pub enabled: Option<bool>,
}
