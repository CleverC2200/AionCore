use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_api_types::{
    GeaInteractionPresentation, GeaInteractionRequest, GeaInteractionRequestKind, GeaInteractionRequestReceiptStatus,
    GeaInteractionRequestSnapshot, GeaInteractionRequestStatus, InteractionRequestChangedPayload,
    InteractionRequestList, InteractionRequestReceipt, InteractionRequestSource, InteractionRequestView,
    MessageStatusChangedPayload, WebSocketMessage,
};
use aionui_common::{fnv1a_hex8, now_ms};
use aionui_db::models::MessageRow;
use aionui_db::{
    IConversationRepository, IInteractionRequestRepository, ReceiptResumeClaim, StoreInteractionRequestReceiptParams,
    StoredGeaSessionBootstrap, StoredInteractionRequest, StoredInteractionRequestReceipt,
    StoredUnfinalizedInteractionRequestReceipt, UpsertGeaSessionBootstrapParams, UpsertInteractionRequestParams,
};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::InteractionTurnResolver;
use crate::error::GeaError;

const CHANGED_EVENT: &str = "interactionRequest.changed";
pub(super) const RESUME_CLAIM_LEASE_MS: i64 = 30_000;

#[derive(Clone)]
pub(super) struct InteractionRequestProjection {
    interaction_repo: Arc<dyn IInteractionRequestRepository>,
    conversation_repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    turn_resolver: Option<InteractionTurnResolver>,
    action_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl InteractionRequestProjection {
    pub(super) fn new(
        interaction_repo: Arc<dyn IInteractionRequestRepository>,
        conversation_repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        turn_resolver: Option<InteractionTurnResolver>,
    ) -> Self {
        Self {
            interaction_repo,
            conversation_repo,
            broadcaster,
            turn_resolver,
            action_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) async fn action_lock(&self, user_id: &str, request_id: &str) -> Arc<Mutex<()>> {
        let key = format!("{user_id}\0{request_id}");
        let mut locks = self.action_locks.lock().await;
        locks.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    }

    pub(super) async fn store_session_bootstrap(
        &self,
        user_id: &str,
        conversation_id: &str,
        consumer_code: &str,
        preparation_id: Option<String>,
    ) -> Result<(), GeaError> {
        self.interaction_repo
            .upsert_session_bootstrap(&UpsertGeaSessionBootstrapParams {
                user_id: user_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                consumer_code: consumer_code.to_owned(),
                preparation_id,
                updated_at: now_ms(),
            })
            .await
            .map_err(storage_error)
    }

    pub(super) async fn pending_session_bootstraps(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredGeaSessionBootstrap>, GeaError> {
        self.interaction_repo
            .list_pending_session_bootstraps(user_id)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn reconcile_snapshot(
        &self,
        user_id: &str,
        conversation_id: &str,
        snapshot: &GeaInteractionRequestSnapshot,
    ) -> Result<(), GeaError> {
        self.ensure_conversation_owner(user_id, conversation_id).await?;
        let existing = self
            .interaction_repo
            .list_for_conversation(user_id, conversation_id)
            .await
            .map_err(storage_error)?;
        let existing_by_id: HashMap<&str, &StoredInteractionRequest> =
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

            self.interaction_repo
                .upsert(&UpsertInteractionRequestParams {
                    user_id: user_id.to_owned(),
                    request_id: request.id.clone(),
                    conversation_id: conversation_id.to_owned(),
                    version: request.version.clone(),
                    status: "pending".to_owned(),
                    kind: kind_name(request.kind).to_owned(),
                    title: request.title.clone(),
                    summary: request.summary.clone(),
                    source_label: request.source_label.clone(),
                    allowed_actions,
                    expires_at: request.expires_at.clone(),
                    updated_at: request.updated_at.clone(),
                    presentation,
                    upstream_revision: snapshot.revision.clone(),
                    turn_id: active_turn_id.clone(),
                    message_id: message_id.clone(),
                    changed_at: now_ms(),
                })
                .await
                .map_err(storage_error)?;

            messages.push(message_row(conversation_id, &message_id, request)?);
        }

        for message in messages {
            self.conversation_repo
                .upsert_message(user_id, &message)
                .await
                .map_err(storage_error)?;
            if changed {
                self.broadcast_message(user_id, &message);
            }
        }
        for row in existing
            .iter()
            .filter(|row| row.status == "pending" && !incoming_ids.contains(row.request_id.as_str()))
        {
            self.finish_message(user_id, row).await?;
        }
        let incoming_request_ids = snapshot.items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        self.interaction_repo
            .resolve_missing(user_id, conversation_id, &incoming_request_ids, now_ms())
            .await
            .map_err(storage_error)?;
        if changed {
            self.broadcast_changed(user_id).await?;
        }
        Ok(())
    }

    pub(super) async fn list_pending(&self, user_id: &str) -> Result<InteractionRequestList, GeaError> {
        let rows = self
            .interaction_repo
            .list_pending(user_id)
            .await
            .map_err(storage_error)?;
        let items = rows.iter().map(row_to_view).collect::<Result<Vec<_>, _>>()?;
        let revision = revision_for(&items)?;
        Ok(InteractionRequestList { revision, items })
    }

    pub(super) async fn find(&self, user_id: &str, request_id: &str) -> Result<InteractionRequestView, GeaError> {
        let row = self.find_row(user_id, request_id).await?;
        row_to_view(&row)
    }

    pub(super) async fn load_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<StoredInteractionRequestReceipt>, GeaError> {
        self.interaction_repo
            .load_receipt(user_id, request_id, idempotency_key)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn load_equivalent_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        expected_version: &str,
        action_id: &str,
    ) -> Result<Option<StoredInteractionRequestReceipt>, GeaError> {
        self.interaction_repo
            .load_equivalent_receipt(user_id, request_id, expected_version, action_id)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn list_unfinalized_receipts(
        &self,
        user_id: &str,
    ) -> Result<Vec<StoredUnfinalizedInteractionRequestReceipt>, GeaError> {
        self.interaction_repo
            .list_unfinalized_receipts(user_id)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn store_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        expected_version: &str,
        action_id: &str,
        receipt: &InteractionRequestReceipt,
    ) -> Result<(), GeaError> {
        let encoded = serde_json::to_string(receipt).map_err(storage_json_error)?;
        self.interaction_repo
            .store_receipt(&StoreInteractionRequestReceiptParams {
                user_id: user_id.to_owned(),
                request_id: request_id.to_owned(),
                idempotency_key: idempotency_key.to_owned(),
                expected_version: expected_version.to_owned(),
                action_id: action_id.to_owned(),
                receipt: encoded,
                created_at: now_ms(),
            })
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    pub(super) async fn claim_receipt_resume(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
    ) -> Result<ReceiptResumeClaim, GeaError> {
        let claimed_at = now_ms();
        self.interaction_repo
            .claim_receipt_resume(
                user_id,
                request_id,
                idempotency_key,
                claim_owner,
                claimed_at,
                claimed_at.saturating_sub(RESUME_CLAIM_LEASE_MS),
            )
            .await
            .map_err(storage_error)
    }

    pub(super) async fn mark_receipt_resume_started(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
    ) -> Result<(), GeaError> {
        let marked = self
            .interaction_repo
            .mark_receipt_resume_started(user_id, request_id, idempotency_key, claim_owner, now_ms())
            .await
            .map_err(storage_error)?;
        if !marked {
            return Err(GeaError::conflict(
                "GEA_INTERACTION_RESUME_CLAIM_LOST",
                "待办结果恢复租约已失效，请稍后重试",
            ));
        }
        Ok(())
    }

    pub(super) async fn mark_receipt_resume_delivered(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        claim_owner: &str,
    ) -> Result<(), GeaError> {
        let marked = self
            .interaction_repo
            .mark_receipt_resume_delivered(user_id, request_id, idempotency_key, claim_owner, now_ms())
            .await
            .map_err(storage_error)?;
        if !marked {
            return Err(GeaError::conflict(
                "GEA_INTERACTION_RESUME_CLAIM_LOST",
                "待办结果恢复租约已失效，请稍后重试",
            ));
        }
        Ok(())
    }

    pub(super) async fn finalize_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        receipt: &InteractionRequestReceipt,
        require_resume_delivered: bool,
    ) -> Result<(), GeaError> {
        let status = match receipt.status {
            GeaInteractionRequestReceiptStatus::Accepted | GeaInteractionRequestReceiptStatus::AlreadyResolved => {
                Some("resolved")
            }
            GeaInteractionRequestReceiptStatus::Expired => Some("expired"),
            GeaInteractionRequestReceiptStatus::Forbidden | GeaInteractionRequestReceiptStatus::Conflict => None,
            GeaInteractionRequestReceiptStatus::UnknownExternalWrite => Some("verification_required"),
        };
        if let Some(status) = status {
            self.interaction_repo
                .update_status(user_id, request_id, status, &receipt.version, now_ms())
                .await
                .map_err(storage_error)?;
        }
        if status.is_some_and(|status| status != "pending") {
            let row = self.find_row(user_id, request_id).await?;
            self.finish_message(user_id, &row).await?;
        }
        let finalized = self
            .interaction_repo
            .mark_receipt_finalized(user_id, request_id, idempotency_key, require_resume_delivered, now_ms())
            .await
            .map_err(storage_error)?;
        if !finalized {
            return Err(GeaError::conflict(
                "GEA_INTERACTION_RESUME_NOT_DELIVERED",
                "待办结果尚未恢复原 Turn，请稍后重试",
            ));
        }
        self.broadcast_changed(user_id).await?;
        Ok(())
    }

    pub(super) async fn apply_authoritative_request(
        &self,
        user_id: &str,
        request: &GeaInteractionRequest,
    ) -> Result<InteractionRequestView, GeaError> {
        let row = self.find_row(user_id, &request.id).await?;
        let allowed_actions = serde_json::to_string(&request.allowed_actions).map_err(storage_json_error)?;
        let presentation = serde_json::to_string(&request.presentation).map_err(storage_json_error)?;
        self.interaction_repo
            .update_authoritative(
                user_id,
                &request.id,
                &UpsertInteractionRequestParams {
                    user_id: user_id.to_owned(),
                    request_id: request.id.clone(),
                    conversation_id: row.conversation_id.clone(),
                    version: request.version.clone(),
                    status: status_name(request.status).to_owned(),
                    kind: kind_name(request.kind).to_owned(),
                    title: request.title.clone(),
                    summary: request.summary.clone(),
                    source_label: request.source_label.clone(),
                    allowed_actions,
                    expires_at: request.expires_at.clone(),
                    updated_at: request.updated_at.clone(),
                    presentation,
                    upstream_revision: String::new(),
                    turn_id: row.turn_id.clone(),
                    message_id: row.message_id.clone(),
                    changed_at: now_ms(),
                },
            )
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

    async fn find_row(&self, user_id: &str, request_id: &str) -> Result<StoredInteractionRequest, GeaError> {
        self.interaction_repo
            .find(user_id, request_id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| GeaError::not_found("Interaction Request 不存在"))
    }

    async fn ensure_conversation_owner(&self, user_id: &str, conversation_id: &str) -> Result<(), GeaError> {
        let exists = self
            .interaction_repo
            .conversation_exists(user_id, conversation_id)
            .await
            .map_err(storage_error)?;
        if !exists {
            return Err(GeaError::not_found("GEA 会话关联的本地对话不存在"));
        }
        Ok(())
    }

    async fn finish_message(&self, user_id: &str, row: &StoredInteractionRequest) -> Result<(), GeaError> {
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
        let payload = InteractionRequestChangedPayload {
            user_id: user_id.to_owned(),
            revision,
        };
        let value = serde_json::to_value(payload).map_err(storage_json_error)?;
        self.broadcaster.broadcast(WebSocketMessage::new(CHANGED_EVENT, value));
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

fn row_to_view(row: &StoredInteractionRequest) -> Result<InteractionRequestView, GeaError> {
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
    tracing::error!(error = %error, "Interaction Request storage operation failed");
    GeaError::internal("Interaction Request 存储失败")
}

fn storage_json_error(error: impl std::fmt::Display) -> GeaError {
    tracing::error!(error = %error, "Interaction Request JSON operation failed");
    storage_corrupt("Interaction Request 数据无效")
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
    use aionui_db::{SqliteConversationRepository, SqliteInteractionRequestRepository, init_database_memory};
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
        let conversation_repo = Arc::new(SqliteConversationRepository::new(database.pool().clone()));
        let interaction_repo = Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone()));
        let bus = Arc::new(BroadcastEventBus::new(32));
        let projection = InteractionRequestProjection::new(interaction_repo, conversation_repo, bus.clone(), None);
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
        assert_eq!(events.recv().await.unwrap().name, "interactionRequest.changed");

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

        sqlx::query("DELETE FROM messages WHERE type = 'ask'")
            .execute(database.pool())
            .await
            .unwrap();
        projection
            .reconcile_snapshot("system_default_user", "conversation-1", &snapshot)
            .await
            .unwrap();
        let healed_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(healed_count, 1, "an unchanged snapshot must heal a missing message row");
        assert!(
            events.try_recv().is_err(),
            "self-healing must not emit a duplicate event"
        );
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
        assert_eq!(events.recv().await.unwrap().name, "interactionRequest.changed");
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
            Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone())),
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
    async fn resolved_message_finalization_retries_after_a_storage_failure() {
        let (projection, database, _bus) = fixture().await;
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
        sqlx::query(
            "CREATE TRIGGER fail_interaction_message_finish \
             BEFORE UPDATE OF status ON messages WHEN NEW.status = 'finish' \
             BEGIN SELECT RAISE(FAIL, 'simulated message finish failure'); END",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let empty = GeaInteractionRequestSnapshot {
            revision: "r2".to_owned(),
            items: vec![],
        };

        projection
            .reconcile_snapshot("system_default_user", "conversation-1", &empty)
            .await
            .unwrap_err();
        assert_eq!(
            projection
                .list_pending("system_default_user")
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        sqlx::query("DROP TRIGGER fail_interaction_message_finish")
            .execute(database.pool())
            .await
            .unwrap();

        projection
            .reconcile_snapshot("system_default_user", "conversation-1", &empty)
            .await
            .unwrap();
        assert!(
            projection
                .list_pending("system_default_user")
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let message_status: String = sqlx::query_scalar("SELECT status FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(message_status, "finish");
    }

    #[tokio::test]
    async fn projection_captures_the_active_turn_anchor() {
        let (_projection, database, bus) = fixture().await;
        let projection = InteractionRequestProjection::new(
            Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone())),
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

    #[tokio::test]
    async fn authoritative_request_replaces_the_current_projection() {
        let (projection, _database, _bus) = fixture().await;
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
        let mut authoritative = question("v2");
        authoritative.title = "Choose the corrected cost center".to_owned();

        let view = projection
            .apply_authoritative_request("system_default_user", &authoritative)
            .await
            .unwrap();

        assert_eq!(view.version, "v2");
        assert_eq!(view.title, "Choose the corrected cost center");
        assert_eq!(view.status, GeaInteractionRequestStatus::Pending);
    }
}
