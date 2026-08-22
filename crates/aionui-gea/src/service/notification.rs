use std::collections::HashMap;
use std::sync::{Arc, Weak};

use aionui_api_types::{
    GeaNotificationSnapshot, NotificationChangedPayload, NotificationChangedReason, NotificationKind, NotificationList,
    NotificationReceipt, NotificationSeverity, NotificationStatus, NotificationSyncState, NotificationTarget,
    NotificationView, WebSocketMessage,
};
use aionui_common::now_ms;
use aionui_db::{
    INotificationRepository, ReplaceNotificationSnapshotParams, StoreNotificationReceiptParams, StoredNotification,
    UpsertNotificationParams,
};
use aionui_realtime::EventBroadcaster;
use tokio::sync::{Mutex, RwLock};

use crate::error::GeaError;

const CHANGED_EVENT: &str = "notification.changed";

#[derive(Clone)]
pub(super) struct NotificationProjection {
    repo: Arc<dyn INotificationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    action_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    sync_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
    mutation_gates: Arc<Mutex<HashMap<String, Weak<RwLock<()>>>>>,
    sync_states: Arc<Mutex<HashMap<String, (NotificationSyncState, Vec<String>)>>>,
}

impl NotificationProjection {
    pub(super) fn new(repo: Arc<dyn INotificationRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            repo,
            broadcaster,
            action_locks: Arc::new(Mutex::new(HashMap::new())),
            sync_locks: Arc::new(Mutex::new(HashMap::new())),
            mutation_gates: Arc::new(Mutex::new(HashMap::new())),
            sync_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) async fn sync_lock(&self, user_id: &str, tenant_id: &str) -> Arc<Mutex<()>> {
        let key = format!("{user_id}\0{tenant_id}");
        let mut locks = self.sync_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    pub(super) async fn set_sync_state(
        &self,
        user_id: &str,
        tenant_id: &str,
        state: NotificationSyncState,
        failure_codes: Vec<String>,
    ) {
        self.sync_states
            .lock()
            .await
            .insert(format!("{user_id}\0{tenant_id}"), (state, failure_codes));
    }

    pub(super) async fn action_lock(&self, user_id: &str, tenant_id: &str, notification_id: &str) -> Arc<Mutex<()>> {
        let key = format!("{user_id}\0{tenant_id}\0{notification_id}");
        let mut locks = self.action_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    pub(super) async fn mutation_gate(&self, user_id: &str, tenant_id: &str) -> Arc<RwLock<()>> {
        let key = format!("{user_id}\0{tenant_id}");
        let mut gates = self.mutation_gates.lock().await;
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(RwLock::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }

    pub(super) async fn reconcile_snapshot(
        &self,
        user_id: &str,
        tenant_id: &str,
        snapshot: &GeaNotificationSnapshot,
        trace_id: &str,
    ) -> Result<bool, GeaError> {
        let mutation_gate = self.mutation_gate(user_id, tenant_id).await;
        let _mutation_guard = mutation_gate.write().await;
        let before = self
            .repo
            .list(user_id, tenant_id, Some("all"))
            .await
            .map_err(storage_error)?;
        let before_by_id = before
            .iter()
            .map(|item| (item.notification_id.as_str(), item))
            .collect::<HashMap<_, _>>();
        let created_count = snapshot
            .items
            .iter()
            .filter(|item| !before_by_id.contains_key(item.id.as_str()))
            .count();
        let updated_count = snapshot
            .items
            .iter()
            .filter(|item| {
                before_by_id.get(item.id.as_str()).is_some_and(|stored| {
                    stored.version != item.version || stored.status != enum_json(item.status).unwrap_or_default()
                })
            })
            .count();
        let incoming_ids = snapshot
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let removed_count = before
            .iter()
            .filter(|item| !incoming_ids.contains(item.notification_id.as_str()))
            .count();
        let items = snapshot
            .items
            .iter()
            .map(|item| {
                Ok(UpsertNotificationParams {
                    notification_id: item.id.clone(),
                    version: item.version.clone(),
                    status: enum_json(item.status)?,
                    kind: enum_json(item.kind)?,
                    severity: enum_json(item.severity)?,
                    title: item.title.clone(),
                    summary: item.summary.clone(),
                    body: item.body.clone(),
                    dismissible: item.dismissible,
                    source: item.source.clone(),
                    target: serde_json::to_string(&item.target).map_err(storage_json_error)?,
                    interaction_request_id: item.interaction_request_id.clone(),
                    created_at: item.created_at.clone(),
                    expires_at: item.expires_at.clone(),
                })
            })
            .collect::<Result<Vec<_>, GeaError>>()?;
        let changed = self
            .repo
            .replace_snapshot(&ReplaceNotificationSnapshotParams {
                user_id: user_id.to_owned(),
                tenant_id: tenant_id.to_owned(),
                revision: snapshot.revision.clone(),
                items,
                synced_at: now_ms(),
            })
            .await
            .map_err(storage_error)?;
        if changed {
            self.broadcast_changed(
                user_id,
                &snapshot.revision,
                NotificationChangedReason::Snapshot,
                None,
                Some(trace_id.to_owned()),
            )?;
            tracing::info!(
                event = "notification.projection.reconciled",
                trace_id,
                revision = %snapshot.revision,
                created_count,
                updated_count,
                removed_count,
                result = "committed",
                "GEA Notification projection reconciled"
            );
        }
        Ok(changed)
    }

    pub(super) async fn list(
        &self,
        user_id: &str,
        tenant_id: &str,
        status: Option<&str>,
    ) -> Result<NotificationList, GeaError> {
        let rows = self
            .repo
            .list(user_id, tenant_id, status)
            .await
            .map_err(storage_error)?;
        let scope = self.repo.scope(user_id, tenant_id).await.map_err(storage_error)?;
        let hide_expired = matches!(status, None | Some("active"));
        let items = rows
            .iter()
            .filter(|row| !hide_expired || !is_expired(row.expires_at.as_deref()))
            .map(row_to_view)
            .collect::<Result<Vec<_>, _>>()?;
        let mut list = NotificationList {
            revision: scope.as_ref().map(|value| value.revision.clone()).unwrap_or_default(),
            items,
            sync_state: if scope.is_some() {
                NotificationSyncState::Fresh
            } else {
                NotificationSyncState::Idle
            },
            last_synced_at: scope.and_then(|value| {
                chrono::DateTime::from_timestamp_millis(value.last_synced_at).map(|value| value.to_rfc3339())
            }),
            failure_codes: Vec::new(),
        };
        if let Some((state, failure_codes)) = self
            .sync_states
            .lock()
            .await
            .get(&format!("{user_id}\0{tenant_id}"))
            .cloned()
        {
            list.sync_state = state;
            list.failure_codes = failure_codes;
        }
        Ok(list)
    }

    pub(super) async fn find(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
    ) -> Result<NotificationView, GeaError> {
        let row = self
            .repo
            .find(user_id, tenant_id, notification_id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| GeaError::notification_not_found("Notification 不存在"))?;
        row_to_view(&row)
    }

    pub(super) async fn load_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        idempotency_key: &str,
        expected_version: &str,
        action: &str,
    ) -> Result<Option<NotificationReceipt>, GeaError> {
        let stored = self
            .repo
            .load_receipt(user_id, tenant_id, notification_id, idempotency_key)
            .await
            .map_err(storage_error)?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        if stored.expected_version != expected_version || stored.action != action {
            return Err(GeaError::invalid_request(
                "同一 idempotencyKey 不能用于不同 Notification 版本或动作",
            ));
        }
        serde_json::from_str(&stored.receipt)
            .map(Some)
            .map_err(storage_json_error)
    }

    pub(super) async fn load_equivalent_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        expected_version: &str,
        action: &str,
    ) -> Result<Option<NotificationReceipt>, GeaError> {
        self.repo
            .load_equivalent_receipt(user_id, tenant_id, notification_id, expected_version, action)
            .await
            .map_err(storage_error)?
            .map(|stored| serde_json::from_str(&stored.receipt).map_err(storage_json_error))
            .transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn store_receipt(
        &self,
        user_id: &str,
        tenant_id: &str,
        notification_id: &str,
        expected_version: &str,
        idempotency_key: &str,
        action: &str,
        receipt: &NotificationReceipt,
        trace_id: &str,
    ) -> Result<(), GeaError> {
        self.repo
            .store_receipt_and_update(&StoreNotificationReceiptParams {
                user_id: user_id.to_owned(),
                tenant_id: tenant_id.to_owned(),
                notification_id: notification_id.to_owned(),
                idempotency_key: idempotency_key.to_owned(),
                expected_version: expected_version.to_owned(),
                action: action.to_owned(),
                receipt: serde_json::to_string(receipt).map_err(storage_json_error)?,
                created_at: now_ms(),
                version: receipt.version.clone(),
                status: enum_json(receipt.status)?,
            })
            .await
            .map_err(storage_error)?;
        self.broadcast_changed(
            user_id,
            &receipt.version,
            if receipt.status == NotificationStatus::Dismissed {
                NotificationChangedReason::Dismissed
            } else {
                NotificationChangedReason::Read
            },
            Some(notification_id.to_owned()),
            Some(trace_id.to_owned()),
        )
    }

    fn broadcast_changed(
        &self,
        user_id: &str,
        revision: &str,
        reason: NotificationChangedReason,
        notification_id: Option<String>,
        trace_id: Option<String>,
    ) -> Result<(), GeaError> {
        tracing::info!(
            event = "notification.event.emitted",
            revision,
            reason = ?reason,
            notification_id = notification_id.as_deref().unwrap_or(""),
            trace_id = trace_id.as_deref().unwrap_or(""),
            "GEA Notification invalidation emitted"
        );
        let payload = NotificationChangedPayload {
            user_id: user_id.to_owned(),
            revision: revision.to_owned(),
            reason,
            notification_id,
            trace_id,
        };
        self.broadcaster.broadcast(WebSocketMessage::new(
            CHANGED_EVENT,
            serde_json::to_value(payload).map_err(storage_json_error)?,
        ));
        Ok(())
    }
}

fn row_to_view(row: &StoredNotification) -> Result<NotificationView, GeaError> {
    Ok(NotificationView {
        id: row.notification_id.clone(),
        version: row.version.clone(),
        status: parse_enum(&row.status)?,
        kind: parse_enum::<NotificationKind>(&row.kind)?,
        severity: parse_enum::<NotificationSeverity>(&row.severity)?,
        title: row.title.clone(),
        summary: row.summary.clone(),
        body: row.body.clone(),
        dismissible: row.dismissible,
        source: row.source.clone(),
        target: serde_json::from_str::<NotificationTarget>(&row.target).map_err(storage_json_error)?,
        interaction_request_id: row.interaction_request_id.clone(),
        created_at: row.created_at.clone(),
        expires_at: row.expires_at.clone(),
    })
}

fn enum_json<T: serde::Serialize>(value: T) -> Result<String, GeaError> {
    serde_json::to_value(value)
        .map_err(storage_json_error)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| storage_json_error("enum did not serialize as a string"))
}

fn parse_enum<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, GeaError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(storage_json_error)
}

fn is_expired(expires_at: Option<&str>) -> bool {
    expires_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|value| value <= chrono::Utc::now())
}

fn storage_error(error: impl std::fmt::Display) -> GeaError {
    tracing::error!(error = %error, "GEA Notification projection storage failed");
    GeaError::server_error("GEA_NOTIFICATION_STORAGE_ERROR", "Notification 投影不可用")
}

fn storage_json_error(error: impl std::fmt::Display) -> GeaError {
    tracing::error!(error = %error, "GEA Notification projection JSON is invalid");
    GeaError::server_error("GEA_NOTIFICATION_STORAGE_ERROR", "Notification 投影格式无效")
}
