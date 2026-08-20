use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_api_types::{
    GeaInteractionPresentation, GeaInteractionRequest, GeaInteractionRequestKind, GeaInteractionRequestReceiptStatus,
    GeaInteractionRequestSnapshot, GeaInteractionRequestStatus, InteractionRequestChangedPayload,
    InteractionRequestList, InteractionRequestReceipt, InteractionRequestSource, InteractionRequestSyncState,
    InteractionRequestView, MessageStatusChangedPayload, WebSocketMessage,
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

    pub(super) async fn session_bootstraps(&self, user_id: &str) -> Result<Vec<StoredGeaSessionBootstrap>, GeaError> {
        self.interaction_repo
            .list_session_bootstraps(user_id)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn reconcile_snapshot(
        &self,
        user_id: &str,
        conversation_id: &str,
        snapshot: &GeaInteractionRequestSnapshot,
    ) -> Result<bool, GeaError> {
        self.ensure_conversation_owner(user_id, conversation_id).await?;
        let existing = self
            .interaction_repo
            .list_for_user(user_id)
            .await
            .map_err(storage_error)?;
        let existing_by_id: HashMap<&str, &StoredInteractionRequest> =
            existing.iter().map(|row| (row.request_id.as_str(), row)).collect();
        let incoming_ids: HashSet<&str> = snapshot.items.iter().map(|item| item.id.as_str()).collect();
        let mut changed = existing
            .iter()
            .any(|row| row.active && !incoming_ids.contains(row.request_id.as_str()));
        let mut messages = Vec::with_capacity(snapshot.items.len());

        for request in &snapshot.items {
            let allowed_actions = serde_json::to_string(&request.allowed_actions).map_err(storage_json_error)?;
            let presentation = serde_json::to_string(&request.presentation).map_err(storage_json_error)?;
            let prior = existing_by_id.get(request.id.as_str()).copied();
            let target_conversation_id = prior.map_or(conversation_id, |row| row.conversation_id.as_str());
            let active_turn_id = prior.and_then(|row| row.turn_id.clone()).or_else(|| {
                self.turn_resolver
                    .as_ref()
                    .and_then(|resolve| resolve(target_conversation_id))
            });
            let active = is_open_status(request.status);
            let kind_changed = prior.is_some_and(|row| row.kind != kind_name(request.kind));
            let message_id = match prior {
                Some(row) if !kind_changed => row.message_id.clone(),
                _ => format!("gea_ir_{}", Uuid::now_v7()),
            };
            if let Some(row) = prior {
                if kind_changed {
                    self.finish_message(user_id, row).await?;
                } else if active && !row.active {
                    self.reactivate_message(user_id, row).await?;
                } else if !active && row.active {
                    self.finish_message(user_id, row).await?;
                }
            }
            changed |= prior.is_none_or(|row| {
                row.version != request.version
                    || row.status != status_name(request.status)
                    || row.active != active
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
                    conversation_id: target_conversation_id.to_owned(),
                    version: request.version.clone(),
                    status: status_name(request.status).to_owned(),
                    active,
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

            messages.push(message_row(target_conversation_id, &message_id, request)?);
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
            .filter(|row| row.active && !incoming_ids.contains(row.request_id.as_str()))
        {
            self.finish_message(user_id, row).await?;
        }
        let incoming_request_ids = snapshot.items.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
        self.interaction_repo
            .deactivate_missing(user_id, &incoming_request_ids, &snapshot.revision, now_ms())
            .await
            .map_err(storage_error)?;
        if changed {
            self.broadcast_changed(user_id).await?;
        }
        Ok(changed)
    }

    pub(super) async fn list_active(&self, user_id: &str) -> Result<InteractionRequestList, GeaError> {
        let rows = self
            .interaction_repo
            .list_active(user_id)
            .await
            .map_err(storage_error)?;
        let items = rows.iter().map(row_to_view).collect::<Result<Vec<_>, _>>()?;
        let revision = revision_for(&items)?;
        Ok(InteractionRequestList {
            revision,
            items,
            sync_state: InteractionRequestSyncState::Complete,
            failed_session_count: 0,
            failure_codes: Vec::new(),
        })
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
        let update = match receipt.status {
            GeaInteractionRequestReceiptStatus::Accepted | GeaInteractionRequestReceiptStatus::AlreadyResolved => {
                Some(("resolved", false))
            }
            GeaInteractionRequestReceiptStatus::Processing => Some(("processing", true)),
            GeaInteractionRequestReceiptStatus::Failed => Some(("pending", true)),
            GeaInteractionRequestReceiptStatus::Expired => Some(("expired", false)),
            GeaInteractionRequestReceiptStatus::Forbidden | GeaInteractionRequestReceiptStatus::Conflict => None,
            GeaInteractionRequestReceiptStatus::Cancelled => Some(("cancelled", false)),
            GeaInteractionRequestReceiptStatus::UnknownExternalWrite => Some(("verification_required", true)),
        };
        if let Some((status, active)) = update {
            self.interaction_repo
                .update_status(user_id, request_id, status, &receipt.version, active, now_ms())
                .await
                .map_err(storage_error)?;
            let row = self.find_row(user_id, request_id).await?;
            let request = row_to_request(&row)?;
            let message = message_row(&row.conversation_id, &row.message_id, &request)?;
            self.conversation_repo
                .upsert_message(user_id, &message)
                .await
                .map_err(storage_error)?;
            if active {
                self.broadcast_message(user_id, &message);
            } else {
                self.finish_message(user_id, &row).await?;
            }
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
        let active = is_open_status(request.status);
        let kind_changed = row.kind != kind_name(request.kind);
        let message_id = if kind_changed {
            self.finish_message(user_id, &row).await?;
            format!("gea_ir_{}", Uuid::now_v7())
        } else {
            if active && !row.active {
                self.reactivate_message(user_id, &row).await?;
            }
            row.message_id.clone()
        };
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
                    active,
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
                    message_id: message_id.clone(),
                    changed_at: now_ms(),
                },
            )
            .await
            .map_err(storage_error)?;
        let message = message_row(&row.conversation_id, &message_id, request)?;
        self.conversation_repo
            .upsert_message(user_id, &message)
            .await
            .map_err(storage_error)?;
        if active {
            self.broadcast_message(user_id, &message);
        } else {
            let updated = self.find_row(user_id, &request.id).await?;
            self.finish_message(user_id, &updated).await?;
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

    async fn reactivate_message(&self, user_id: &str, row: &StoredInteractionRequest) -> Result<(), GeaError> {
        self.conversation_repo
            .update_message(
                user_id,
                &row.conversation_id,
                &row.message_id,
                &aionui_db::MessageRowUpdate {
                    content: None,
                    status: Some(Some("work".to_owned())),
                    hidden: None,
                },
            )
            .await
            .map_err(storage_error)
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
        let revision = self.list_active(user_id).await?.revision;
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
                "title": request.title,
                "summary": request.summary,
                "interaction_request": {
                    "id": request.id,
                    "version": request.version,
                    "status": request.status,
                    "allowed_actions": request.allowed_actions,
                },
                "questions": questions,
            }),
        ),
        GeaInteractionPresentation::Permission {
            title,
            description,
            operation,
            detail,
            options,
        } => {
            let options = if options.is_empty() {
                request
                    .allowed_actions
                    .iter()
                    .map(|value| json!({ "label": value, "value": value }))
                    .collect::<Vec<_>>()
            } else {
                options.iter().map(|option| json!(option)).collect::<Vec<_>>()
            };
            (
                "permission",
                json!({
                "id": request.id,
                "call_id": request.id,
                "title": if title.is_empty() { &request.title } else { title },
                "description": if description.is_empty() { request.summary.as_deref().unwrap_or("") } else { description },
                "action": if operation.is_empty() { "interaction_request" } else { operation },
                "command_type": detail,
                "interaction_request": {
                    "id": request.id,
                    "version": request.version,
                    "status": request.status,
                    "allowed_actions": request.allowed_actions,
                },
                "options": options,
                }),
            )
        }
    };
    Ok(MessageRow {
        id: message_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        msg_id: Some(message_id.to_owned()),
        r#type: message_type.to_owned(),
        content: serde_json::to_string(&content).map_err(storage_json_error)?,
        position: Some("left".to_owned()),
        status: Some(
            if is_open_status(request.status) {
                "work"
            } else {
                "finish"
            }
            .to_owned(),
        ),
        hidden: false,
        created_at: now_ms(),
        backend_turn_id: None,
    })
}

fn row_to_view(row: &StoredInteractionRequest) -> Result<InteractionRequestView, GeaError> {
    let request = row_to_request(row)?;
    Ok(InteractionRequestView {
        id: request.id,
        version: request.version,
        kind: request.kind,
        status: request.status,
        title: request.title,
        summary: request.summary,
        source: InteractionRequestSource {
            r#type: "business_system".to_owned(),
            label: request.source_label,
        },
        conversation_id: row.conversation_id.clone(),
        team_id: None,
        slot_id: None,
        turn_id: row.turn_id.clone(),
        message_id: Some(row.message_id.clone()),
        expires_at: request.expires_at,
        allowed_actions: request.allowed_actions,
        updated_at: (!request.updated_at.is_empty()).then_some(request.updated_at),
        stale: false,
    })
}

fn row_to_request(row: &StoredInteractionRequest) -> Result<GeaInteractionRequest, GeaError> {
    let kind = match row.kind.as_str() {
        "question" => GeaInteractionRequestKind::Question,
        "permission" => GeaInteractionRequestKind::Permission,
        _ => return Err(storage_corrupt("未知 Interaction Request kind")),
    };
    let status = match row.status.as_str() {
        "pending" => GeaInteractionRequestStatus::Pending,
        "processing" => GeaInteractionRequestStatus::Processing,
        "resolved" => GeaInteractionRequestStatus::Resolved,
        "expired" => GeaInteractionRequestStatus::Expired,
        "cancelled" => GeaInteractionRequestStatus::Cancelled,
        "verification_required" => GeaInteractionRequestStatus::VerificationRequired,
        _ => return Err(storage_corrupt("未知 Interaction Request status")),
    };
    Ok(GeaInteractionRequest {
        id: row.request_id.clone(),
        version: row.version.clone(),
        kind,
        status,
        title: row.title.clone(),
        summary: row.summary.clone(),
        expires_at: row.expires_at.clone(),
        source_label: row.source_label.clone(),
        allowed_actions: serde_json::from_str(&row.allowed_actions).map_err(storage_json_error)?,
        updated_at: row.updated_at.clone(),
        presentation: serde_json::from_str(&row.presentation).map_err(storage_json_error)?,
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
        GeaInteractionRequestStatus::Processing => "processing",
        GeaInteractionRequestStatus::Resolved => "resolved",
        GeaInteractionRequestStatus::Expired => "expired",
        GeaInteractionRequestStatus::Cancelled => "cancelled",
        GeaInteractionRequestStatus::VerificationRequired => "verification_required",
    }
}

fn is_open_status(status: GeaInteractionRequestStatus) -> bool {
    matches!(
        status,
        GeaInteractionRequestStatus::Pending
            | GeaInteractionRequestStatus::Processing
            | GeaInteractionRequestStatus::VerificationRequired
    )
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
        GeaInteractionPermissionOption, GeaInteractionPresentation, GeaInteractionQuestion,
        GeaInteractionQuestionOption, GeaInteractionRequest, GeaInteractionRequestKind,
        GeaInteractionRequestReceiptStatus, GeaInteractionRequestSnapshot, GeaInteractionRequestStatus,
        InteractionRequestReceipt,
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
        let list = projection.list_active("system_default_user").await.unwrap();
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
    async fn empty_snapshot_deactivates_without_inventing_status_and_can_reappear() {
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
                .list_active("system_default_user")
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
        let (business_status, active): (String, bool) =
            sqlx::query_as("SELECT status, active FROM gea_interaction_requests WHERE request_id = 'request-1'")
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(business_status, "pending");
        assert!(!active, "snapshot absence must not invent a terminal business status");
        let (source_revision, disposition): (String, String) = sqlx::query_as(
            "SELECT source_revision, disposition \
             FROM gea_interaction_request_projection_audit WHERE request_id = 'request-1'",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(source_revision, "r2");
        assert_eq!(disposition, "absent_from_gea_active_snapshot");

        let restarted = InteractionRequestProjection::new(
            Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone())),
            Arc::new(SqliteConversationRepository::new(database.pool().clone())),
            Arc::new(BroadcastEventBus::new(8)),
            None,
        );
        assert!(
            restarted
                .list_active("system_default_user")
                .await
                .unwrap()
                .items
                .is_empty()
        );

        let mut reappeared = question("v2");
        reappeared.title = "Choose the corrected cost center".to_owned();
        restarted
            .reconcile_snapshot(
                "system_default_user",
                "conversation-1",
                &GeaInteractionRequestSnapshot {
                    revision: "r3".to_owned(),
                    items: vec![reappeared],
                },
            )
            .await
            .unwrap();
        let active = restarted.list_active("system_default_user").await.unwrap();
        assert_eq!(active.items.len(), 1);
        assert_eq!(active.items[0].version, "v2");
        assert_eq!(active.items[0].title, "Choose the corrected cost center");
        let message_status: String = sqlx::query_scalar("SELECT status FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(message_status, "work");
    }

    #[tokio::test]
    async fn missing_source_timestamps_use_stable_request_id_order_after_timed_items() {
        let (projection, _database, _bus) = fixture().await;
        let mut timed = question("v1");
        timed.id = "request-timed".to_owned();
        let mut missing_b = question("v1");
        missing_b.id = "request-b".to_owned();
        missing_b.updated_at.clear();
        let mut missing_a = question("v1");
        missing_a.id = "request-a".to_owned();
        missing_a.updated_at.clear();

        projection
            .reconcile_snapshot(
                "system_default_user",
                "conversation-1",
                &GeaInteractionRequestSnapshot {
                    revision: "r1".to_owned(),
                    items: vec![missing_b, timed, missing_a],
                },
            )
            .await
            .unwrap();

        let ids = projection
            .list_active("system_default_user")
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["request-timed", "request-a", "request-b"]);
    }

    #[tokio::test]
    async fn complete_snapshot_preserves_the_first_navigation_anchor_across_sessions() {
        let (projection, database, _bus) = fixture().await;
        sqlx::query(
            "INSERT INTO conversations \
                (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
             VALUES ('conversation-2', 'system_default_user', 'GEA fixture 2', 'aionrs', '{}', 'running', 0, 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
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

        let mut processing = question("v2");
        processing.status = GeaInteractionRequestStatus::Processing;
        projection
            .reconcile_snapshot(
                "system_default_user",
                "conversation-2",
                &GeaInteractionRequestSnapshot {
                    revision: "r2".to_owned(),
                    items: vec![processing],
                },
            )
            .await
            .unwrap();

        let (conversation_id, status, active, message_id): (String, String, bool, String) = sqlx::query_as(
            "SELECT conversation_id, status, active, message_id FROM gea_interaction_requests WHERE request_id = 'request-1'",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(conversation_id, "conversation-1");
        assert_eq!(status, "processing");
        assert!(active);
        let (message_conversation_id, message_status): (String, String) =
            sqlx::query_as("SELECT conversation_id, status FROM messages WHERE id = ?")
                .bind(message_id)
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(message_conversation_id, "conversation-1");
        assert_eq!(message_status, "work");
        assert!(
            projection
                .list_active("system_default_user")
                .await
                .unwrap()
                .items
                .is_empty(),
            "processing is synchronized but not actionable"
        );
        let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(message_count, 1);
    }

    #[tokio::test]
    async fn processing_receipt_updates_the_persisted_card_version_and_status() {
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
        let receipt = InteractionRequestReceipt {
            receipt_id: "receipt-processing".to_owned(),
            request_id: "request-1".to_owned(),
            version: "v2".to_owned(),
            status: GeaInteractionRequestReceiptStatus::Processing,
            turn_continuation: None,
            resolved_at: None,
            resolved_by: None,
            request: None,
        };
        projection
            .store_receipt(
                "system_default_user",
                "request-1",
                "processing-action",
                "v1",
                "answer",
                &receipt,
            )
            .await
            .unwrap();

        projection
            .finalize_receipt("system_default_user", "request-1", "processing-action", &receipt, false)
            .await
            .unwrap();

        let (status, active): (String, bool) =
            sqlx::query_as("SELECT status, active FROM gea_interaction_requests WHERE request_id = 'request-1'")
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(status, "processing");
        assert!(active);
        let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE type = 'ask'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        let content: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(content["interaction_request"]["version"], "v2");
        assert_eq!(content["interaction_request"]["status"], "processing");
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
            projection.list_active("system_default_user").await.unwrap().items.len(),
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
                .list_active("system_default_user")
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

        let list = projection.list_active("system_default_user").await.unwrap();
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

    #[tokio::test]
    async fn authoritative_kind_change_finishes_the_old_card_and_creates_the_new_card() {
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
        let old_message_id = projection.list_active("system_default_user").await.unwrap().items[0]
            .message_id
            .clone()
            .unwrap();
        let mut verification = question("v2");
        verification.status = GeaInteractionRequestStatus::VerificationRequired;
        verification.kind = GeaInteractionRequestKind::Permission;
        verification.title = "Verify the external write".to_owned();
        verification.allowed_actions = vec!["verify_succeeded".to_owned(), "verify_failed".to_owned()];
        verification.presentation = GeaInteractionPresentation::Permission {
            title: "Verify the external write".to_owned(),
            description: "Confirm the external result.".to_owned(),
            operation: "verify".to_owned(),
            detail: None,
            options: vec![
                GeaInteractionPermissionOption {
                    label: "Succeeded".to_owned(),
                    value: "verify_succeeded".to_owned(),
                },
                GeaInteractionPermissionOption {
                    label: "Failed".to_owned(),
                    value: "verify_failed".to_owned(),
                },
            ],
        };

        let view = projection
            .apply_authoritative_request("system_default_user", &verification)
            .await
            .unwrap();

        let new_message_id = view.message_id.unwrap();
        assert_ne!(new_message_id, old_message_id);
        let old_status: String = sqlx::query_scalar("SELECT status FROM messages WHERE id = ?")
            .bind(old_message_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(old_status, "finish");
        let (new_type, new_status): (String, String) = sqlx::query_as("SELECT type, status FROM messages WHERE id = ?")
            .bind(new_message_id)
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!((new_type.as_str(), new_status.as_str()), ("permission", "work"));
    }
}
