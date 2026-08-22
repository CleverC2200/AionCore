use async_trait::async_trait;
use sqlx::SqlitePool;

use super::notification::{INotificationRepository, ReplaceNotificationSnapshotParams, StoreNotificationReceiptParams};
use crate::DbError;
use crate::models::{StoredNotification, StoredNotificationReceipt, StoredNotificationScope};

#[derive(Clone)]
pub struct SqliteNotificationRepository {
    pool: SqlitePool,
}

impl SqliteNotificationRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_NOTIFICATION: &str = "SELECT notification_id, version, status, kind, severity, title, summary, body, \
    dismissible, source, target, interaction_request_id, created_at, expires_at FROM gea_notifications";

#[async_trait]
impl INotificationRepository for SqliteNotificationRepository {
    async fn replace_snapshot(&self, params: &ReplaceNotificationSnapshotParams) -> Result<bool, DbError> {
        let current: Option<String> =
            sqlx::query_scalar("SELECT revision FROM gea_notification_scopes WHERE user_id = ? AND tenant_id = ?")
                .bind(&params.user_id)
                .bind(&params.tenant_id)
                .fetch_optional(&self.pool)
                .await?;
        if current.as_deref() == Some(params.revision.as_str()) {
            return Ok(false);
        }

        let mut transaction = self.pool.begin().await?;
        for item in &params.items {
            sqlx::query(
                "INSERT INTO gea_notifications \
                    (user_id, tenant_id, notification_id, version, status, kind, severity, title, summary, body, \
                     dismissible, source, target, interaction_request_id, created_at, expires_at, upstream_revision, changed_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(user_id, tenant_id, notification_id) DO UPDATE SET \
                    version = excluded.version, status = excluded.status, kind = excluded.kind, \
                    severity = excluded.severity, title = excluded.title, summary = excluded.summary, body = excluded.body, \
                    dismissible = excluded.dismissible, source = excluded.source, target = excluded.target, \
                    interaction_request_id = excluded.interaction_request_id, created_at = excluded.created_at, \
                    expires_at = excluded.expires_at, upstream_revision = excluded.upstream_revision, \
                    changed_at = excluded.changed_at",
            )
            .bind(&params.user_id)
            .bind(&params.tenant_id)
            .bind(&item.notification_id)
            .bind(&item.version)
            .bind(&item.status)
            .bind(&item.kind)
            .bind(&item.severity)
            .bind(&item.title)
            .bind(&item.summary)
            .bind(&item.body)
            .bind(item.dismissible)
            .bind(&item.source)
            .bind(&item.target)
            .bind(&item.interaction_request_id)
            .bind(&item.created_at)
            .bind(&item.expires_at)
            .bind(&params.revision)
            .bind(params.synced_at)
            .execute(&mut *transaction)
            .await?;
        }

        if params.items.is_empty() {
            sqlx::query("DELETE FROM gea_notifications WHERE user_id = ? AND tenant_id = ?")
                .bind(&params.user_id)
                .bind(&params.tenant_id)
                .execute(&mut *transaction)
                .await?;
        } else {
            let placeholders = std::iter::repeat_n("?", params.items.len())
                .collect::<Vec<_>>()
                .join(",");
            let statement = format!(
                "DELETE FROM gea_notifications WHERE user_id = ? AND tenant_id = ? \
                 AND notification_id NOT IN ({placeholders})"
            );
            let mut query = sqlx::query(&statement).bind(&params.user_id).bind(&params.tenant_id);
            for item in &params.items {
                query = query.bind(&item.notification_id);
            }
            query.execute(&mut *transaction).await?;
        }

        sqlx::query(
            "INSERT INTO gea_notification_scopes (user_id, tenant_id, revision, last_synced_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id, tenant_id) DO UPDATE SET revision = excluded.revision, \
             last_synced_at = excluded.last_synced_at",
        )
        .bind(&params.user_id)
        .bind(&params.tenant_id)
        .bind(&params.revision)
        .bind(params.synced_at)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(true)
    }

    async fn scope(&self, user_id: &str, tenant_id: &str) -> Result<Option<StoredNotificationScope>, DbError> {
        Ok(sqlx::query_as(
            "SELECT revision, last_synced_at FROM gea_notification_scopes WHERE user_id = ? AND tenant_id = ?",
        )
        .bind(user_id)
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list(
        &self,
        user_id: &str,
        tenant_id: &str,
        status: Option<&str>,
    ) -> Result<Vec<StoredNotification>, DbError> {
        let (condition, status_value) = match status {
            None | Some("active") => ("status != 'dismissed'", None),
            Some("all") => ("1 = 1", None),
            Some(value) => ("status = ?", Some(value)),
        };
        let statement = format!(
            "{SELECT_NOTIFICATION} WHERE user_id = ? AND tenant_id = ? AND {condition} \
             ORDER BY created_at DESC, notification_id ASC"
        );
        let mut query = sqlx::query_as::<_, StoredNotification>(&statement)
            .bind(user_id)
            .bind(tenant_id);
        if let Some(value) = status_value {
            query = query.bind(value);
        }
        Ok(query.fetch_all(&self.pool).await?)
    }

    async fn find(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
    ) -> Result<Option<StoredNotification>, DbError> {
        Ok(sqlx::query_as::<_, StoredNotification>(&format!(
            "{SELECT_NOTIFICATION} WHERE user_id = ? AND tenant_id = ? AND notification_id = ?"
        ))
        .bind(user_id)
        .bind(tenant_id)
        .bind(notification_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn load_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<StoredNotificationReceipt>, DbError> {
        Ok(sqlx::query_as(
            "SELECT idempotency_key, expected_version, action, receipt FROM gea_notification_receipts \
             WHERE user_id = ? AND tenant_id = ? AND notification_id = ? AND idempotency_key = ?",
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(notification_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn load_equivalent_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        expected_version: &str,
        action: &str,
    ) -> Result<Option<StoredNotificationReceipt>, DbError> {
        Ok(sqlx::query_as(
            "SELECT idempotency_key, expected_version, action, receipt FROM gea_notification_receipts \
             WHERE user_id = ? AND tenant_id = ? AND notification_id = ? AND expected_version = ? AND action = ?",
        )
        .bind(user_id)
        .bind(tenant_id)
        .bind(notification_id)
        .bind(expected_version)
        .bind(action)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn store_receipt_and_update(&self, params: &StoreNotificationReceiptParams) -> Result<(), DbError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT OR IGNORE INTO gea_notification_receipts \
                (user_id, tenant_id, notification_id, idempotency_key, expected_version, action, receipt, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&params.user_id)
        .bind(&params.tenant_id)
        .bind(&params.notification_id)
        .bind(&params.idempotency_key)
        .bind(&params.expected_version)
        .bind(&params.action)
        .bind(&params.receipt)
        .bind(params.created_at)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE gea_notifications SET version = ?, status = ?, changed_at = ? \
             WHERE user_id = ? AND tenant_id = ? AND notification_id = ?",
        )
        .bind(&params.version)
        .bind(&params.status)
        .bind(params.created_at)
        .bind(&params.user_id)
        .bind(&params.tenant_id)
        .bind(&params.notification_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}
