use aionui_common::TimestampMs;

use crate::DbError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredTeamWorkTask {
    pub id: String,
    pub team_id: String,
    pub status: String,
    pub version: i64,
    pub blocked_by: String,
    pub payload: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredTeamWorkRun {
    pub id: String,
    pub team_id: String,
    pub task_id: String,
    pub attempt: i64,
    pub status: String,
    pub payload: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredTeamWorkEvent {
    pub sequence: i64,
    pub event_id: String,
    pub team_id: String,
    pub task_id: String,
    pub run_id: Option<String>,
    pub name: String,
    pub task_version: i64,
    pub payload: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct CreateTeamWorkTaskParams {
    pub task: StoredTeamWorkTask,
    pub event_id: String,
    pub event_name: String,
    pub event_payload: String,
}

#[derive(Debug, Clone)]
pub struct PersistTeamWorkCommandParams {
    pub user_id: String,
    pub task: StoredTeamWorkTask,
    pub expected_version: u64,
    pub run: Option<StoredTeamWorkRun>,
    pub idempotency_key: String,
    pub envelope: String,
    pub receipt: String,
    pub event_id: String,
    pub event_name: String,
    pub event_payload: String,
    pub event_created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistTeamWorkCommandResult {
    Applied { sequence: i64, receipt: String },
    Duplicate { envelope: String, receipt: String },
    VersionConflict { actual_version: u64 },
}

#[async_trait::async_trait]
pub trait ITeamWorkRepository: Send + Sync {
    async fn create_task(&self, user_id: &str, params: &CreateTeamWorkTaskParams) -> Result<i64, DbError>;
    async fn get_task(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
    ) -> Result<Option<StoredTeamWorkTask>, DbError>;
    async fn list_tasks(&self, user_id: &str, team_id: &str) -> Result<Vec<StoredTeamWorkTask>, DbError>;
    async fn list_runs(&self, user_id: &str, team_id: &str) -> Result<Vec<StoredTeamWorkRun>, DbError>;
    async fn list_events(
        &self,
        user_id: &str,
        team_id: &str,
        after_sequence: i64,
        limit: i64,
    ) -> Result<Vec<StoredTeamWorkEvent>, DbError>;
    async fn latest_sequence(&self, user_id: &str, team_id: &str) -> Result<i64, DbError>;
    async fn get_command(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<(String, String)>, DbError>;
    async fn unlock_ready_tasks(
        &self,
        user_id: &str,
        team_id: &str,
        updated_at: TimestampMs,
    ) -> Result<Vec<StoredTeamWorkEvent>, DbError>;
    async fn persist_command(
        &self,
        params: &PersistTeamWorkCommandParams,
    ) -> Result<PersistTeamWorkCommandResult, DbError>;
}
