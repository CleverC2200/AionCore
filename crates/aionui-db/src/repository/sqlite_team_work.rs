use sqlx::SqlitePool;

use aionui_common::generate_prefixed_id;

use crate::DbError;
use crate::repository::team_work::{
    CreateTeamWorkTaskParams, ITeamWorkRepository, PersistTeamWorkCommandParams, PersistTeamWorkCommandResult,
    StoredTeamWorkEvent, StoredTeamWorkRun, StoredTeamWorkTask,
};

#[derive(Clone, Debug)]
pub struct SqliteTeamWorkRepository {
    pool: SqlitePool,
}

impl SqliteTeamWorkRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ITeamWorkRepository for SqliteTeamWorkRepository {
    async fn create_task(&self, user_id: &str, params: &CreateTeamWorkTaskParams) -> Result<i64, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let owns_team: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM teams WHERE id = ? AND user_id = ?)")
                .bind(&params.task.team_id)
                .bind(user_id)
                .fetch_one(&mut *connection)
                .await?;
            if !owns_team {
                return Err(DbError::NotFound("team not found".into()));
            }
            sqlx::query(
                "INSERT INTO team_work_tasks (id, team_id, status, version, blocked_by, payload, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&params.task.id)
            .bind(&params.task.team_id)
            .bind(&params.task.status)
            .bind(params.task.version)
            .bind(&params.task.blocked_by)
            .bind(&params.task.payload)
            .bind(params.task.created_at)
            .bind(params.task.updated_at)
            .execute(&mut *connection)
            .await?;
            let sequence: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM team_work_events WHERE team_id = ?",
            )
            .bind(&params.task.team_id)
            .fetch_one(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO team_work_events \
                 (sequence, event_id, team_id, task_id, run_id, name, task_version, payload, created_at) \
                 VALUES (?, ?, ?, ?, NULL, ?, ?, ?, ?)",
            )
            .bind(sequence)
            .bind(&params.event_id)
            .bind(&params.task.team_id)
            .bind(&params.task.id)
            .bind(&params.event_name)
            .bind(params.task.version)
            .bind(&params.event_payload)
            .bind(params.task.created_at)
            .execute(&mut *connection)
            .await?;
            Ok(sequence)
        }
        .await;
        finish_transaction(&mut connection, result).await
    }

    async fn get_task(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
    ) -> Result<Option<StoredTeamWorkTask>, DbError> {
        Ok(sqlx::query_as::<_, StoredTeamWorkTask>(
            "SELECT wt.* FROM team_work_tasks wt JOIN teams t ON t.id = wt.team_id \
             WHERE t.user_id = ? AND wt.team_id = ? AND wt.id = ?",
        )
        .bind(user_id)
        .bind(team_id)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_tasks(&self, user_id: &str, team_id: &str) -> Result<Vec<StoredTeamWorkTask>, DbError> {
        Ok(sqlx::query_as::<_, StoredTeamWorkTask>(
            "SELECT wt.* FROM team_work_tasks wt JOIN teams t ON t.id = wt.team_id \
             WHERE t.user_id = ? AND wt.team_id = ? ORDER BY wt.created_at, wt.id",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_runs(&self, user_id: &str, team_id: &str) -> Result<Vec<StoredTeamWorkRun>, DbError> {
        Ok(sqlx::query_as::<_, StoredTeamWorkRun>(
            "SELECT wr.* FROM team_work_runs wr JOIN teams t ON t.id = wr.team_id \
             WHERE t.user_id = ? AND wr.team_id = ? ORDER BY wr.created_at, wr.id",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_events(
        &self,
        user_id: &str,
        team_id: &str,
        after_sequence: i64,
        limit: i64,
    ) -> Result<Vec<StoredTeamWorkEvent>, DbError> {
        Ok(sqlx::query_as::<_, StoredTeamWorkEvent>(
            "SELECT we.* FROM team_work_events we JOIN teams t ON t.id = we.team_id \
             WHERE t.user_id = ? AND we.team_id = ? AND we.sequence > ? \
             ORDER BY we.sequence ASC LIMIT ?",
        )
        .bind(user_id)
        .bind(team_id)
        .bind(after_sequence)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn latest_sequence(&self, user_id: &str, team_id: &str) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT COALESCE(MAX(we.sequence), 0) FROM team_work_events we JOIN teams t ON t.id = we.team_id \
             WHERE t.user_id = ? AND we.team_id = ?",
        )
        .bind(user_id)
        .bind(team_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn get_command(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<(String, String)>, DbError> {
        Ok(sqlx::query_as::<_, (String, String)>(
            "SELECT wc.envelope, wc.receipt FROM team_work_commands wc JOIN teams t ON t.id = wc.team_id \
             WHERE t.user_id = ? AND wc.team_id = ? AND wc.task_id = ? AND wc.idempotency_key = ?",
        )
        .bind(user_id)
        .bind(team_id)
        .bind(task_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn unlock_ready_tasks(
        &self,
        user_id: &str,
        team_id: &str,
        updated_at: i64,
    ) -> Result<Vec<StoredTeamWorkEvent>, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let candidates = sqlx::query_as::<_, StoredTeamWorkTask>(
                "SELECT wt.* FROM team_work_tasks wt JOIN teams t ON t.id = wt.team_id \
                 WHERE t.user_id = ? AND wt.team_id = ? AND wt.status = 'backlog' \
                 AND NOT EXISTS (\
                   SELECT 1 FROM json_each(wt.blocked_by) dependency \
                   LEFT JOIN team_work_tasks blocker ON blocker.id = dependency.value AND blocker.team_id = wt.team_id \
                   WHERE blocker.id IS NULL OR blocker.status != 'done'\
                 ) ORDER BY wt.created_at, wt.id",
            )
            .bind(user_id)
            .bind(team_id)
            .fetch_all(&mut *connection)
            .await?;
            let mut sequence: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(sequence), 0) FROM team_work_events WHERE team_id = ?")
                    .bind(team_id)
                    .fetch_one(&mut *connection)
                    .await?;
            let mut events = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                let version = candidate.version.saturating_add(1);
                let mut task: serde_json::Value = serde_json::from_str(&candidate.payload)
                    .map_err(|error| DbError::Init(format!("invalid team work task payload: {error}")))?;
                task["status"] = serde_json::Value::String("ready".into());
                task["version"] = serde_json::Value::from(version);
                task["updated_at"] = serde_json::Value::from(updated_at);
                let payload = serde_json::to_string(&task)
                    .map_err(|error| DbError::Init(format!("invalid team work task payload: {error}")))?;
                sqlx::query(
                    "UPDATE team_work_tasks SET status = 'ready', version = ?, payload = ?, updated_at = ? \
                     WHERE id = ? AND team_id = ? AND version = ? AND status = 'backlog'",
                )
                .bind(version)
                .bind(&payload)
                .bind(updated_at)
                .bind(&candidate.id)
                .bind(team_id)
                .bind(candidate.version)
                .execute(&mut *connection)
                .await?;
                sequence = sequence.saturating_add(1);
                let event_id = generate_prefixed_id("work_event");
                let event_payload = serde_json::to_string(&serde_json::json!({
                    "task": task,
                    "run": null,
                    "attention": null,
                }))
                .map_err(|error| DbError::Init(format!("invalid team work event payload: {error}")))?;
                sqlx::query(
                    "INSERT INTO team_work_events \
                     (sequence, event_id, team_id, task_id, run_id, name, task_version, payload, created_at) \
                     VALUES (?, ?, ?, ?, NULL, 'team.workTaskUnlocked', ?, ?, ?)",
                )
                .bind(sequence)
                .bind(&event_id)
                .bind(team_id)
                .bind(&candidate.id)
                .bind(version)
                .bind(&event_payload)
                .bind(updated_at)
                .execute(&mut *connection)
                .await?;
                events.push(StoredTeamWorkEvent {
                    sequence,
                    event_id,
                    team_id: team_id.to_owned(),
                    task_id: candidate.id,
                    run_id: None,
                    name: "team.workTaskUnlocked".into(),
                    task_version: version,
                    payload: event_payload,
                    created_at: updated_at,
                });
            }
            Ok(events)
        }
        .await;
        finish_transaction(&mut connection, result).await
    }

    async fn persist_command(
        &self,
        params: &PersistTeamWorkCommandParams,
    ) -> Result<PersistTeamWorkCommandResult, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let existing = sqlx::query_as::<_, (String, String)>(
                "SELECT wc.envelope, wc.receipt FROM team_work_commands wc JOIN teams t ON t.id = wc.team_id \
                 WHERE t.user_id = ? AND wc.team_id = ? AND wc.task_id = ? AND wc.idempotency_key = ?",
            )
            .bind(&params.user_id)
            .bind(&params.task.team_id)
            .bind(&params.task.id)
            .bind(&params.idempotency_key)
            .fetch_optional(&mut *connection)
            .await?;
            if let Some((envelope, receipt)) = existing {
                return Ok(PersistTeamWorkCommandResult::Duplicate { envelope, receipt });
            }

            let updated = sqlx::query(
                "UPDATE team_work_tasks SET status = ?, version = ?, blocked_by = ?, payload = ?, updated_at = ? \
                 WHERE id = ? AND team_id = ? AND version = ? \
                 AND EXISTS(SELECT 1 FROM teams WHERE id = ? AND user_id = ?)",
            )
            .bind(&params.task.status)
            .bind(params.task.version)
            .bind(&params.task.blocked_by)
            .bind(&params.task.payload)
            .bind(params.task.updated_at)
            .bind(&params.task.id)
            .bind(&params.task.team_id)
            .bind(params.expected_version as i64)
            .bind(&params.task.team_id)
            .bind(&params.user_id)
            .execute(&mut *connection)
            .await?;
            if updated.rows_affected() != 1 {
                let actual_version = sqlx::query_scalar::<_, i64>(
                    "SELECT wt.version FROM team_work_tasks wt JOIN teams t ON t.id = wt.team_id \
                     WHERE t.user_id = ? AND wt.team_id = ? AND wt.id = ?",
                )
                .bind(&params.user_id)
                .bind(&params.task.team_id)
                .bind(&params.task.id)
                .fetch_optional(&mut *connection)
                .await?
                .ok_or_else(|| DbError::NotFound("team work task not found".into()))?;
                return Ok(PersistTeamWorkCommandResult::VersionConflict {
                    actual_version: actual_version as u64,
                });
            }

            if let Some(run) = &params.run {
                sqlx::query(
                    "INSERT INTO team_work_runs (id, team_id, task_id, attempt, status, payload, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT(id) DO UPDATE SET status = excluded.status, payload = excluded.payload, updated_at = excluded.updated_at",
                )
                .bind(&run.id)
                .bind(&run.team_id)
                .bind(&run.task_id)
                .bind(run.attempt)
                .bind(&run.status)
                .bind(&run.payload)
                .bind(run.created_at)
                .bind(run.updated_at)
                .execute(&mut *connection)
                .await?;
            }

            let sequence: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM team_work_events WHERE team_id = ?",
            )
            .bind(&params.task.team_id)
            .fetch_one(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO team_work_events \
                 (sequence, event_id, team_id, task_id, run_id, name, task_version, payload, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(sequence)
            .bind(&params.event_id)
            .bind(&params.task.team_id)
            .bind(&params.task.id)
            .bind(params.run.as_ref().map(|run| run.id.as_str()))
            .bind(&params.event_name)
            .bind(params.task.version)
            .bind(&params.event_payload)
            .bind(params.event_created_at)
            .execute(&mut *connection)
            .await?;
            let receipt = sqlx::query_scalar::<_, String>("SELECT json_set(?, '$.event_sequence', ?)")
                .bind(&params.receipt)
                .bind(sequence)
                .fetch_one(&mut *connection)
                .await?;
            sqlx::query(
                "INSERT INTO team_work_commands (team_id, task_id, idempotency_key, envelope, receipt, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&params.task.team_id)
            .bind(&params.task.id)
            .bind(&params.idempotency_key)
            .bind(&params.envelope)
            .bind(&receipt)
            .bind(params.event_created_at)
            .execute(&mut *connection)
            .await?;
            Ok(PersistTeamWorkCommandResult::Applied { sequence, receipt })
        }
        .await;
        finish_transaction(&mut connection, result).await
    }
}

async fn finish_transaction<T>(
    connection: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    result: Result<T, DbError>,
) -> Result<T, DbError> {
    match result {
        Ok(value) => {
            sqlx::query("COMMIT").execute(&mut **connection).await?;
            Ok(value)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut **connection).await;
            Err(error)
        }
    }
}
