use sqlx::SqlitePool;

use crate::DbError;
use crate::models::{
    StoredGeaSessionBootstrap, StoredInteractionRequest, StoredInteractionRequestReceipt,
    StoredUnfinalizedInteractionRequestReceipt,
};
use crate::repository::interaction_request::{
    IInteractionRequestRepository, ReceiptResumeClaim, StoreInteractionRequestReceiptParams,
    UpsertGeaSessionBootstrapParams, UpsertInteractionRequestParams,
};

#[derive(Clone, Debug)]
pub struct SqliteInteractionRequestRepository {
    pool: SqlitePool,
}

impl SqliteInteractionRequestRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_REQUEST: &str = "SELECT request_id, conversation_id, version, status, active, kind, title, summary, source_label, \
     allowed_actions, expires_at, updated_at, presentation, turn_id, message_id \
     FROM gea_interaction_requests";

#[async_trait::async_trait]
impl IInteractionRequestRepository for SqliteInteractionRequestRepository {
    async fn upsert_session_bootstrap(&self, params: &UpsertGeaSessionBootstrapParams) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO gea_interaction_session_bootstraps \
                (user_id, conversation_id, consumer_code, preparation_id, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, conversation_id) DO UPDATE SET \
                consumer_code = excluded.consumer_code, preparation_id = excluded.preparation_id, \
                updated_at = excluded.updated_at",
        )
        .bind(&params.user_id)
        .bind(&params.conversation_id)
        .bind(&params.consumer_code)
        .bind(&params.preparation_id)
        .bind(params.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_session_bootstraps(&self, user_id: &str) -> Result<Vec<StoredGeaSessionBootstrap>, DbError> {
        Ok(sqlx::query_as(
            "SELECT conversation_id, consumer_code, preparation_id \
             FROM gea_interaction_session_bootstraps \
             WHERE user_id = ? ORDER BY conversation_id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn conversation_exists(&self, user_id: &str, conversation_id: &str) -> Result<bool, DbError> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE user_id = ? AND id = ?)")
                .bind(user_id)
                .bind(conversation_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<StoredInteractionRequest>, DbError> {
        Ok(
            sqlx::query_as::<_, StoredInteractionRequest>(&format!("{SELECT_REQUEST} WHERE user_id = ?"))
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn list_active(&self, user_id: &str) -> Result<Vec<StoredInteractionRequest>, DbError> {
        Ok(sqlx::query_as::<_, StoredInteractionRequest>(&format!(
            "{SELECT_REQUEST} WHERE user_id = ? AND active = 1 \
             AND status IN ('pending', 'verification_required') \
             ORDER BY CASE WHEN updated_at = '' THEN 1 ELSE 0 END ASC, updated_at DESC, request_id ASC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn find(&self, user_id: &str, request_id: &str) -> Result<Option<StoredInteractionRequest>, DbError> {
        Ok(sqlx::query_as::<_, StoredInteractionRequest>(&format!(
            "{SELECT_REQUEST} WHERE user_id = ? AND request_id = ?"
        ))
        .bind(user_id)
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn upsert(&self, params: &UpsertInteractionRequestParams) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO gea_interaction_requests \
                (user_id, request_id, conversation_id, version, status, kind, title, summary, source_label, active, \
                 allowed_actions, expires_at, updated_at, presentation, upstream_revision, turn_id, message_id, changed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, request_id) DO UPDATE SET \
                version = excluded.version, status = excluded.status, active = excluded.active, kind = excluded.kind, title = excluded.title, \
                summary = excluded.summary, source_label = excluded.source_label, \
                allowed_actions = excluded.allowed_actions, expires_at = excluded.expires_at, \
                updated_at = excluded.updated_at, presentation = excluded.presentation, \
                upstream_revision = excluded.upstream_revision, \
                turn_id = COALESCE(gea_interaction_requests.turn_id, excluded.turn_id), \
                message_id = excluded.message_id, \
                changed_at = excluded.changed_at",
        )
        .bind(&params.user_id)
        .bind(&params.request_id)
        .bind(&params.conversation_id)
        .bind(&params.version)
        .bind(&params.status)
        .bind(&params.kind)
        .bind(&params.title)
        .bind(&params.summary)
        .bind(&params.source_label)
        .bind(params.active)
        .bind(&params.allowed_actions)
        .bind(&params.expires_at)
        .bind(&params.updated_at)
        .bind(&params.presentation)
        .bind(&params.upstream_revision)
        .bind(&params.turn_id)
        .bind(&params.message_id)
        .bind(params.changed_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn deactivate_missing(
        &self,
        user_id: &str,
        incoming_request_ids: &[String],
        source_revision: &str,
        changed_at: i64,
    ) -> Result<(), DbError> {
        let mut transaction = self.pool.begin().await?;
        if incoming_request_ids.is_empty() {
            sqlx::query(
                "INSERT OR IGNORE INTO gea_interaction_request_projection_audit \
                    (user_id, request_id, conversation_id, local_status, source_revision, disposition, recorded_at) \
                 SELECT user_id, request_id, conversation_id, status, ?, \
                        'absent_from_gea_active_snapshot', ? \
                 FROM gea_interaction_requests \
                 WHERE user_id = ? AND active = 1",
            )
            .bind(source_revision)
            .bind(changed_at)
            .bind(user_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE gea_interaction_requests SET active = 0, changed_at = ? \
                 WHERE user_id = ? AND active = 1",
            )
            .bind(changed_at)
            .bind(user_id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Ok(());
        }
        let placeholders = std::iter::repeat_n("?", incoming_request_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let audit_statement = format!(
            "INSERT OR IGNORE INTO gea_interaction_request_projection_audit \
                (user_id, request_id, conversation_id, local_status, source_revision, disposition, recorded_at) \
             SELECT user_id, request_id, conversation_id, status, ?, \
                    'absent_from_gea_active_snapshot', ? \
             FROM gea_interaction_requests \
             WHERE user_id = ? AND active = 1 \
             AND request_id NOT IN ({placeholders})"
        );
        let mut audit_query = sqlx::query(&audit_statement)
            .bind(source_revision)
            .bind(changed_at)
            .bind(user_id);
        for request_id in incoming_request_ids {
            audit_query = audit_query.bind(request_id);
        }
        audit_query.execute(&mut *transaction).await?;

        let update_statement = format!(
            "UPDATE gea_interaction_requests SET active = 0, changed_at = ? \
             WHERE user_id = ? AND active = 1 \
             AND request_id NOT IN ({placeholders})"
        );
        let mut query = sqlx::query(&update_statement).bind(changed_at).bind(user_id);
        for request_id in incoming_request_ids {
            query = query.bind(request_id);
        }
        query.execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn update_authoritative(
        &self,
        user_id: &str,
        request_id: &str,
        update: &UpsertInteractionRequestParams,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE gea_interaction_requests SET version = ?, status = ?, active = ?, kind = ?, title = ?, summary = ?, \
                    source_label = ?, allowed_actions = ?, expires_at = ?, updated_at = ?, presentation = ?, \
                    message_id = ?, changed_at = ? \
             WHERE user_id = ? AND request_id = ?",
        )
        .bind(&update.version)
        .bind(&update.status)
        .bind(update.active)
        .bind(&update.kind)
        .bind(&update.title)
        .bind(&update.summary)
        .bind(&update.source_label)
        .bind(&update.allowed_actions)
        .bind(&update.expires_at)
        .bind(&update.updated_at)
        .bind(&update.presentation)
        .bind(&update.message_id)
        .bind(update.changed_at)
        .bind(user_id)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_status(
        &self,
        user_id: &str,
        request_id: &str,
        status: &str,
        version: &str,
        active: bool,
        changed_at: i64,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE gea_interaction_requests SET status = ?, version = ?, active = ?, changed_at = ? \
             WHERE user_id = ? AND request_id = ?",
        )
        .bind(status)
        .bind(version)
        .bind(active)
        .bind(changed_at)
        .bind(user_id)
        .bind(request_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<StoredInteractionRequestReceipt>, DbError> {
        Ok(sqlx::query_as(
            "SELECT idempotency_key, expected_version, action_id, receipt, resume_claim_owner, resume_claimed_at, \
                    resume_started_at, resume_delivered_at, finalized_at \
             FROM gea_interaction_request_receipts \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ?",
        )
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn load_equivalent_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        expected_version: &str,
        action_id: &str,
    ) -> Result<Option<StoredInteractionRequestReceipt>, DbError> {
        Ok(sqlx::query_as(
            "SELECT idempotency_key, expected_version, action_id, receipt, resume_claim_owner, resume_claimed_at, \
                    resume_started_at, resume_delivered_at, finalized_at \
             FROM gea_interaction_request_receipts \
             WHERE user_id = ? AND request_id = ? AND expected_version = ? AND action_id = ? \
             ORDER BY created_at ASC LIMIT 1",
        )
        .bind(user_id)
        .bind(request_id)
        .bind(expected_version)
        .bind(action_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_unfinalized_receipts(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredUnfinalizedInteractionRequestReceipt>, DbError> {
        Ok(sqlx::query_as(
            "SELECT request_id, idempotency_key, receipt \
             FROM gea_interaction_request_receipts \
             WHERE user_id = ? AND finalized_at IS NULL \
             ORDER BY created_at ASC, request_id ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn store_receipt(&self, params: &StoreInteractionRequestReceiptParams) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO gea_interaction_request_receipts \
                (user_id, request_id, idempotency_key, expected_version, action_id, receipt, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&params.user_id)
        .bind(&params.request_id)
        .bind(&params.idempotency_key)
        .bind(&params.expected_version)
        .bind(&params.action_id)
        .bind(&params.receipt)
        .bind(params.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn claim_receipt_resume(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
        claimed_at: i64,
        stale_before: i64,
    ) -> Result<ReceiptResumeClaim, DbError> {
        let result = sqlx::query(
            "UPDATE gea_interaction_request_receipts SET resume_claim_owner = ?, resume_claimed_at = ? \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ? \
               AND finalized_at IS NULL \
               AND resume_started_at IS NULL AND resume_delivered_at IS NULL \
               AND (resume_claim_owner IS NULL OR resume_claimed_at IS NULL \
                    OR resume_claim_owner = ? OR resume_claimed_at <= ?)",
        )
        .bind(claim_owner)
        .bind(claimed_at)
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .bind(claim_owner)
        .bind(stale_before)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(ReceiptResumeClaim::Acquired);
        }
        let stored: Option<(Option<i64>, Option<i64>)> = sqlx::query_as(
            "SELECT resume_started_at, resume_delivered_at \
             FROM gea_interaction_request_receipts \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ? AND finalized_at IS NULL",
        )
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match stored {
            Some((_, Some(_))) => ReceiptResumeClaim::Delivered,
            Some((Some(_), None)) => ReceiptResumeClaim::Unknown,
            _ => ReceiptResumeClaim::Busy,
        })
    }

    async fn mark_receipt_resume_started(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
        started_at: i64,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE gea_interaction_request_receipts SET resume_started_at = ? \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ? \
               AND resume_claim_owner = ? AND finalized_at IS NULL \
               AND resume_started_at IS NULL AND resume_delivered_at IS NULL",
        )
        .bind(started_at)
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .bind(claim_owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_receipt_resume_delivered(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
        delivered_at: i64,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE gea_interaction_request_receipts SET resume_delivered_at = ? \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ? \
               AND resume_claim_owner = ? AND finalized_at IS NULL AND resume_delivered_at IS NULL",
        )
        .bind(delivered_at)
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .bind(claim_owner)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_receipt_finalized(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        require_resume_delivered: bool,
        finalized_at: i64,
    ) -> Result<bool, DbError> {
        let mut statement = "UPDATE gea_interaction_request_receipts SET finalized_at = ? \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ? AND finalized_at IS NULL"
            .to_owned();
        if require_resume_delivered {
            statement.push_str(" AND resume_delivered_at IS NOT NULL");
        }
        let result = sqlx::query(&statement)
            .bind(finalized_at)
            .bind(user_id)
            .bind(request_id)
            .bind(idempotency_key)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }
}
