use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_api_types::{
    GeaInteractionPresentation, GeaInteractionRequest, GeaInteractionRequestKind, GeaInteractionRequestReceiptStatus,
    GeaInteractionRequestSnapshot, GeaInteractionRequestStatus, InteractionRequestList, InteractionRequestReceipt,
    InteractionRequestSource, InteractionRequestView, MessageStatusChangedPayload, WebSocketMessage,
};
use aionui_common::{fnv1a_hex8, now_ms};
use aionui_db::models::MessageRow;
use aionui_db::{IConversationRepository, SqlitePool};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::InteractionTurnResolver;
use crate::error::GeaError;

const CHANGED_EVENT: &str = "interaction_request.changed";

#[derive(Clone)]
pub(crate) struct InteractionRequestProjection {
    pool: SqlitePool,
    conversation_repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    turn_resolver: Option<InteractionTurnResolver>,
    action_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ProjectionRow {
    request_id: String,
    conversation_id: String,
    version: String,
    status: String,
    kind: String,
    title: String,
    summary: Option<String>,
    source_label: Option<String>,
    allowed_actions: String,
    expires_at: Option<String>,
    updated_at: String,
    presentation: String,
    turn_id: Option<String>,
    message_id: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct StoredReceipt {
    pub expected_version: String,
    pub action_id: String,
    pub receipt: String,
}

impl InteractionRequestProjection {
    pub(crate) fn new(
        pool: SqlitePool,
        conversation_repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        turn_resolver: Option<InteractionTurnResolver>,
    ) -> Self {
        Self {
            pool,
            conversation_repo,
            broadcaster,
            turn_resolver,
            action_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn action_lock(&self, user_id: &str, request_id: &str, idempotency_key: &str) -> Arc<Mutex<()>> {
        let key = format!("{user_id}\0{request_id}\0{idempotency_key}");
        let mut locks = self.action_locks.lock().await;
        locks.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    pub(crate) async fn reconcile_snapshot(
        &self,
        user_id: &str,
        conversation_id: &str,
        snapshot: &GeaInteractionRequestSnapshot,
    ) -> Result<(), GeaError> {
        self.ensure_conversation_owner(user_id, conversation_id).await?;
        let existing = sqlx::query_as::<_, ProjectionRow>(
            "SELECT request_id, conversation_id, version, status, kind, title, summary, source_label, \
                    allowed_actions, expires_at, updated_at, presentation, turn_id, message_id \
             FROM gea_interaction_requests WHERE user_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        let existing_by_id: HashMap<&str, &ProjectionRow> =
            existing.iter().map(|row| (row.request_id.as_str(), row)).collect();
        let incoming_ids: HashSet<&str> = snapshot.items.iter().map(|item| item.id.as_str()).collect();
        let active_turn_id = self.turn_resolver.as_ref().and_then(|resolve| resolve(conversation_id));
        let mut changed = existing
            .iter()
            .any(|row| row.status == "pending" && !incoming_ids.contains(row.request_id.as_str()));
        let mut messages = Vec::with_capacity(snapshot.items.len());

        for request in &snapshot.items {
            let allowed_actions = serde_json::to_string(&request.allowed_actions).map_err(storage_json_error)?;
            let presentation = serde_json::to_string(&request.presentation).map_err(storage_json_error)?;
            let prior = existing_by_id.get(request.id.as_str()).copied();
            let message_id = prior
                .map(|row| row.message_id.clone())
                .unwrap_or_else(|| format!("gea_ir_{}", Uuid::now_v7()));
            changed |= prior.is_none_or(|row| {
                row.version != request.version
                    || row.status != "pending"
                    || row.kind != kind_name(request.kind)
                    || row.title != request.title
                    || row.summary != request.summary
                    || row.source_label != request.source_label
                    || row.allowed_actions != allowed_actions
                    || row.expires_at != request.expires_at
                    || row.updated_at != request.updated_at
                    || row.presentation != presentation
            });

            sqlx::query(
                "INSERT INTO gea_interaction_requests \
                    (user_id, request_id, conversation_id, version, status, kind, title, summary, source_label, \
                     allowed_actions, expires_at, updated_at, presentation, upstream_revision, turn_id, message_id, changed_at) \
                 VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(user_id, request_id) DO UPDATE SET \
                    version = excluded.version, status = 'pending', kind = excluded.kind, title = excluded.title, \
                    summary = excluded.summary, source_label = excluded.source_label, \
                    allowed_actions = excluded.allowed_actions, expires_at = excluded.expires_at, \
                    updated_at = excluded.updated_at, presentation = excluded.presentation, \
                    upstream_revision = excluded.upstream_revision, \
                    turn_id = COALESCE(gea_interaction_requests.turn_id, excluded.turn_id), \
                    changed_at = excluded.changed_at",
            )
            .bind(user_id)
            .bind(&request.id)
            .bind(conversation_id)
            .bind(&request.version)
            .bind(kind_name(request.kind))
            .bind(&request.title)
            .bind(&request.summary)
            .bind(&request.source_label)
            .bind(&allowed_actions)
            .bind(&request.expires_at)
            .bind(&request.updated_at)
            .bind(&presentation)
            .bind(&snapshot.revision)
            .bind(&active_turn_id)
            .bind(&message_id)
            .bind(now_ms())
            .execute(&self.pool)
            .await
            .map_err(storage_error)?;

            messages.push(message_row(conversation_id, &message_id, request)?);
        }

        if !incoming_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", incoming_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let statement = format!(
                "UPDATE gea_interaction_requests SET status = 'resolved', changed_at = ? \
                 WHERE user_id = ? AND conversation_id = ? AND status = 'pending' AND request_id NOT IN ({placeholders})"
            );
            let mut query = sqlx::query(&statement)
                .bind(now_ms())
                .bind(user_id)
                .bind(conversation_id);
            for request_id in &incoming_ids {
                query = query.bind(request_id);
            }
            query.execute(&self.pool).await.map_err(storage_error)?;
        } else {
            sqlx::query(
                "UPDATE gea_interaction_requests SET status = 'resolved', changed_at = ? \
                 WHERE user_id = ? AND conversation_id = ? AND status = 'pending'",
            )
            .bind(now_ms())
            .bind(user_id)
            .bind(conversation_id)
            .execute(&self.pool)
            .await
            .map_err(storage_error)?;
        }

        if changed {
            for message in messages {
                self.conversation_repo
                    .upsert_message(user_id, &message)
                    .await
                    .map_err(storage_error)?;
                self.broadcast_message(user_id, &message);
            }
        }
        for row in existing
            .iter()
            .filter(|row| row.status == "pending" && !incoming_ids.contains(row.request_id.as_str()))
        {
            self.finish_message(user_id, row).await?;
        }
        if changed {
            self.broadcast_changed(user_id).await?;
        }
        Ok(())
    }

    pub(crate) async fn list_pending(&self, user_id: &str) -> Result<InteractionRequestList, GeaError> {
        let rows = sqlx::query_as::<_, ProjectionRow>(
            "SELECT request_id, conversation_id, version, status, kind, title, summary, source_label, \
                    allowed_actions, expires_at, updated_at, presentation, turn_id, message_id \
             FROM gea_interaction_requests WHERE user_id = ? AND status = 'pending' \
             ORDER BY updated_at DESC, request_id ASC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?;
        let items = rows.iter().map(row_to_view).collect::<Result<Vec<_>, _>>()?;
        let revision = revision_for(&items)?;
        Ok(InteractionRequestList { revision, items })
    }

    pub(crate) async fn find(&self, user_id: &str, request_id: &str) -> Result<InteractionRequestView, GeaError> {
        let row = self.find_row(user_id, request_id).await?;
        row_to_view(&row)
    }

    pub(crate) async fn load_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<StoredReceipt>, GeaError> {
        sqlx::query_as::<_, StoredReceipt>(
            "SELECT expected_version, action_id, receipt FROM gea_interaction_request_receipts \
             WHERE user_id = ? AND request_id = ? AND idempotency_key = ?",
        )
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_error)
    }

    pub(crate) async fn store_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        expected_version: &str,
        action_id: &str,
        receipt: &InteractionRequestReceipt,
    ) -> Result<(), GeaError> {
        let status = match receipt.status {
            GeaInteractionRequestReceiptStatus::Accepted | GeaInteractionRequestReceiptStatus::AlreadyResolved => {
                "resolved"
            }
            GeaInteractionRequestReceiptStatus::Expired => "expired",
            GeaInteractionRequestReceiptStatus::Forbidden => "pending",
            GeaInteractionRequestReceiptStatus::Conflict => "pending",
            GeaInteractionRequestReceiptStatus::UnknownExternalWrite => "verification_required",
        };
        sqlx::query(
            "UPDATE gea_interaction_requests SET status = ?, version = ?, changed_at = ? \
             WHERE user_id = ? AND request_id = ?",
        )
        .bind(status)
        .bind(&receipt.version)
        .bind(now_ms())
        .bind(user_id)
        .bind(request_id)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        let encoded = serde_json::to_string(receipt).map_err(storage_json_error)?;
        sqlx::query(
            "INSERT INTO gea_interaction_request_receipts \
                (user_id, request_id, idempotency_key, expected_version, action_id, receipt, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(request_id)
        .bind(idempotency_key)
        .bind(expected_version)
        .bind(action_id)
        .bind(encoded)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        if !matches!(status, "pending") {
            let row = self.find_row(user_id, request_id).await?;
            self.finish_message(user_id, &row).await?;
        }
        self.broadcast_changed(user_id).await?;
        Ok(())
    }

    pub(crate) async fn apply_authoritative_request(
        &self,
        user_id: &str,
        request: &GeaInteractionRequest,
    ) -> Result<InteractionRequestView, GeaError> {
        let row = self.find_row(user_id, &request.id).await?;
        let allowed_actions = serde_json::to_string(&request.allowed_actions).map_err(storage_json_error)?;
        let presentation = serde_json::to_string(&request.presentation).map_err(storage_json_error)?;
        sqlx::query(
            "UPDATE gea_interaction_requests SET version = ?, status = ?, kind = ?, title = ?, summary = ?, \
                    source_label = ?, allowed_actions = ?, expires_at = ?, updated_at = ?, presentation = ?, changed_at = ? \
             WHERE user_id = ? AND request_id = ?",
        )
        .bind(&request.version)
        .bind(status_name(request.status))
        .bind(kind_name(request.kind))
        .bind(&request.title)
        .bind(&request.summary)
        .bind(&request.source_label)
        .bind(&allowed_actions)
        .bind(&request.expires_at)
        .bind(&request.updated_at)
        .bind(&presentation)
        .bind(now_ms())
        .bind(user_id)
        .bind(&request.id)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        let message = message_row(&row.conversation_id, &row.message_id, request)?;
        self.conversation_repo
            .upsert_message(user_id, &message)
            .await
            .map_err(storage_error)?;
        if request.status == GeaInteractionRequestStatus::Pending {
            self.broadcast_message(user_id, &message);
        } else {
            self.finish_message(user_id, &row).await?;
        }
        self.find(user_id, &request.id).await
    }

    async fn find_row(&self, user_id: &str, request_id: &str) -> Result<ProjectionRow, GeaError> {
        sqlx::query_as::<_, ProjectionRow>(
            "SELECT request_id, conversation_id, version, status, kind, title, summary, source_label, \
                    allowed_actions, expires_at, updated_at, presentation, turn_id, message_id \
             FROM gea_interaction_requests WHERE user_id = ? AND request_id = ?",
        )
        .bind(user_id)
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| GeaError::not_found("Interaction Request 不存在"))
    }

    async fn ensure_conversation_owner(&self, user_id: &str, conversation_id: &str) -> Result<(), GeaError> {
        let exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM conversations WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(conversation_id)
            .fetch_one(&self.pool)
            .await
            .map_err(storage_error)?;
        if !exists {
            return Err(GeaError::not_found("GEA 会话关联的本地对话不存在"));
        }
        Ok(())
    }

    async fn finish_message(&self, user_id: &str, row: &ProjectionRow) -> Result<(), GeaError> {
        self.conversation_repo
            .update_message(
                user_id,
                &row.conversation_id,
                &row.message_id,
                &aionui_db::MessageRowUpdate {
                    content: None,
                    status: Some(Some("finish".to_owned())),
                    hidden: None,
                },
            )
            .await
            .map_err(storage_error)?;
        let payload = MessageStatusChangedPayload {
            user_id: user_id.to_owned(),
            conversation_id: row.conversation_id.clone(),
            msg_id: row.message_id.clone(),
            status: "finish".to_owned(),
        };
        let value = serde_json::to_value(payload).map_err(storage_json_error)?;
        self.broadcaster
            .broadcast(WebSocketMessage::new("message.statusChanged", value));
        Ok(())
    }

    fn broadcast_message(&self, user_id: &str, row: &MessageRow) {
        let data = serde_json::from_str::<serde_json::Value>(&row.content).unwrap_or_default();
        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.stream",
            json!({
                "user_id": user_id,
                "conversation_id": row.conversation_id,
                "msg_id": row.msg_id,
                "type": row.r#type,
                "data": data,
                "position": row.position,
                "status": row.status,
                "hidden": row.hidden,
                "replace": true,
            }),
        ));
    }

    async fn broadcast_changed(&self, user_id: &str) -> Result<(), GeaError> {
        let revision = self.list_pending(user_id).await?.revision;
        self.broadcaster.broadcast(WebSocketMessage::new(
            CHANGED_EVENT,
            json!({ "user_id": user_id, "revision": revision }),
        ));
        Ok(())
    }
}

fn message_row(
    conversation_id: &str,
    message_id: &str,
    request: &GeaInteractionRequest,
) -> Result<MessageRow, GeaError> {
    let (message_type, content) = match &request.presentation {
        GeaInteractionPresentation::Question { questions } => (
            "ask",
            json!({
                "request_id": request.id,
                "interaction_request": { "id": request.id, "version": request.version },
                "questions": questions,
            }),
        ),
        GeaInteractionPresentation::Permission {
            title,
            description,
            operation,
            detail,
            options,
        } => (
            "permission",
            json!({
                "id": request.id,
                "call_id": request.id,
                "title": title,
                "description": description,
                "action": operation,
                "command_type": detail,
                "interaction_request": { "id": request.id, "version": request.version },
                "options": options,
            }),
        ),
    };
    Ok(MessageRow {
        id: message_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        msg_id: Some(message_id.to_owned()),
        r#type: message_type.to_owned(),
        content: serde_json::to_string(&content).map_err(storage_json_error)?,
        position: Some("left".to_owned()),
        status: Some("work".to_owned()),
        hidden: false,
        created_at: now_ms(),
        backend_turn_id: None,
    })
}

fn row_to_view(row: &ProjectionRow) -> Result<InteractionRequestView, GeaError> {
    let kind = match row.kind.as_str() {
        "question" => GeaInteractionRequestKind::Question,
        "permission" => GeaInteractionRequestKind::Permission,
        _ => return Err(storage_corrupt("未知 Interaction Request kind")),
    };
    let status = match row.status.as_str() {
        "pending" => GeaInteractionRequestStatus::Pending,
        "resolved" => GeaInteractionRequestStatus::Resolved,
        "expired" => GeaInteractionRequestStatus::Expired,
        "cancelled" => GeaInteractionRequestStatus::Cancelled,
        "verification_required" => GeaInteractionRequestStatus::VerificationRequired,
        _ => return Err(storage_corrupt("未知 Interaction Request status")),
    };
    let allowed_actions = serde_json::from_str(&row.allowed_actions).map_err(storage_json_error)?;
    Ok(InteractionRequestView {
        id: row.request_id.clone(),
        version: row.version.clone(),
        kind,
        status,
        title: row.title.clone(),
        summary: row.summary.clone(),
        source: InteractionRequestSource {
            r#type: "business_system".to_owned(),
            label: row.source_label.clone(),
        },
        conversation_id: row.conversation_id.clone(),
        team_id: None,
        slot_id: None,
        turn_id: row.turn_id.clone(),
        message_id: Some(row.message_id.clone()),
        expires_at: row.expires_at.clone(),
        allowed_actions,
        updated_at: row.updated_at.clone(),
    })
}

fn revision_for(items: &[InteractionRequestView]) -> Result<String, GeaError> {
    let encoded = serde_json::to_vec(items).map_err(storage_json_error)?;
    let hash = fnv1a_hex8(&encoded);
    Ok(format!(
        "projection:{}",
        std::str::from_utf8(&hash).unwrap_or("00000000")
    ))
}

fn kind_name(kind: GeaInteractionRequestKind) -> &'static str {
    match kind {
        GeaInteractionRequestKind::Question => "question",
        GeaInteractionRequestKind::Permission => "permission",
    }
}

fn status_name(status: GeaInteractionRequestStatus) -> &'static str {
    match status {
        GeaInteractionRequestStatus::Pending => "pending",
        GeaInteractionRequestStatus::Resolved => "resolved",
        GeaInteractionRequestStatus::Expired => "expired",
        GeaInteractionRequestStatus::Cancelled => "cancelled",
        GeaInteractionRequestStatus::VerificationRequired => "verification_required",
    }
}

fn storage_error(error: impl std::fmt::Display) -> GeaError {
    GeaError::internal(format!("Interaction Request 存储失败: {error}"))
}

fn storage_json_error(error: impl std::fmt::Display) -> GeaError {
    storage_corrupt(format!("Interaction Request JSON 无效: {error}"))
}

fn storage_corrupt(message: impl Into<String>) -> GeaError {
    GeaError::internal(message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aionui_api_types::{
        GeaInteractionPresentation, GeaInteractionQuestion, GeaInteractionQuestionOption, GeaInteractionRequest,
        GeaInteractionRequestKind, GeaInteractionRequestSnapshot, GeaInteractionRequestStatus,
    };
    use aionui_db::{SqliteConversationRepository, init_database_memory};
    use aionui_realtime::BroadcastEventBus;

    use super::InteractionRequestProjection;

    async fn fixture() -> (
        InteractionRequestProjection,
        aionui_db::Database,
        Arc<BroadcastEventBus>,
    ) {
        let database = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO conversations \
                (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
             VALUES ('conversation-1', 'system_default_user', 'GEA fixture', 'aionrs', '{}', 'running', 0, 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let bus = Arc::new(BroadcastEventBus::new(32));
        let projection = InteractionRequestProjection::new(database.pool().clone(), repo, bus.clone(), None);
        (projection, database, bus)
    }

    fn question(version: &str) -> GeaInteractionRequest {
        GeaInteractionRequest {
            id: "request-1".to_owned(),
            version: version.to_owned(),
            status: GeaInteractionRequestStatus::Pending,
            kind: GeaInteractionRequestKind::Question,
            title: "Choose a cost center".to_owned(),
            summary: Some("ERP requires one answer".to_owned()),
            source_label: Some("ERP".to_owned()),
            allowed_actions: vec!["answer".to_owned(), "decline".to_owned()],
            expires_at: None,
            updated_at: "2026-08-17T10:00:00+08:00".to_owned(),
            presentation: GeaInteractionPresentation::Question {
                questions: vec![GeaInteractionQuestion {
                    header: Some("Cost center".to_owned()),
                    question: "Which cost center?".to_owned(),
                    multi_select: false,
                    options: vec![GeaInteractionQuestionOption {
                        label: "CC-100".to_owned(),
                        description: None,
                    }],
                }],
            },
        }
    }

    #[tokio::test]
    async fn snapshot_creates_one_navigable_message_and_is_idempotent() {
        let (projection, database, bus) = fixture().await;
        let mut events = bus.subscribe();
        let snapshot = GeaInteractionRequestSnapshot {
            revision: "r1".to_owned(),
            items: vec![question("v1")],
        };

        projection
            .reconcile_snapshot("system_default_user", "conversation-1", &snapshot)
            .await
            .unwrap();
        let list = projection.list_pending("system_default_user").await.unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].conversation_id, "conversation-1");
        assert!(list.items[0].message_id.is_some());
        let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(message_count, 1);
        assert_eq!(events.recv().await.unwrap().name, "message.stream");
        assert_eq!(events.recv().await.unwrap().name, "interaction_request.changed");

        projection
            .reconcile_snapshot("system_default_user", "conversation-1", &snapshot)
            .await
            .unwrap();
        assert!(
            events.try_recv().is_err(),
            "unchanged snapshots must not emit duplicate events"
        );
        let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(message_count, 1);
    }

    #[tokio::test]
    async fn empty_snapshot_resolves_projection_and_survives_service_restart() {
        let (projection, database, bus) = fixture().await;
        projection
            .reconcile_snapshot(
                "system_default_user",
                "conversation-1",
                &GeaInteractionRequestSnapshot {
                    revision: "r1".to_owned(),
                    items: vec![question("v1")],
                },
            )
            .await
            .unwrap();
        let mut events = bus.subscribe();
        projection
            .reconcile_snapshot(
                "system_default_user",
                "conversation-1",
                &GeaInteractionRequestSnapshot {
                    revision: "r2".to_owned(),
                    items: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(events.recv().await.unwrap().name, "message.statusChanged");
        assert_eq!(events.recv().await.unwrap().name, "interaction_request.changed");
        assert!(
            projection
                .list_pending("system_default_user")
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let status: String = sqlx::query_scalar("SELECT status FROM messages LIMIT 1")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(status, "finish");

        let restarted = InteractionRequestProjection::new(
            database.pool().clone(),
            Arc::new(SqliteConversationRepository::new(database.pool().clone())),
            Arc::new(BroadcastEventBus::new(8)),
            None,
        );
        assert!(
            restarted
                .list_pending("system_default_user")
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[tokio::test]
    async fn projection_captures_the_active_turn_anchor() {
        let (_projection, database, bus) = fixture().await;
        let projection = InteractionRequestProjection::new(
            database.pool().clone(),
            Arc::new(SqliteConversationRepository::new(database.pool().clone())),
            bus,
            Some(Arc::new(|conversation_id| {
                (conversation_id == "conversation-1").then(|| "turn-active-1".to_owned())
            })),
        );
        projection
            .reconcile_snapshot(
                "system_default_user",
                "conversation-1",
                &GeaInteractionRequestSnapshot {
                    revision: "r1".to_owned(),
                    items: vec![question("v1")],
                },
            )
            .await
            .unwrap();

        let list = projection.list_pending("system_default_user").await.unwrap();
        assert_eq!(list.items[0].turn_id.as_deref(), Some("turn-active-1"));
    }
}
