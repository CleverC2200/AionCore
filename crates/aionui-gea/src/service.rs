use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_api_types::{
    CreateGeaSessionRequest, GeaAuthSessionStatus, GeaInteractionRequestActionCommand, GeaInteractionRequestReceipt,
    GeaInteractionRequestReceiptStatus, GeaInteractionRequestSnapshot, GeaNotificationSnapshot, GeaSessionResponse,
    GeaToolCallResponse, GeaToolInfo, InteractionRequestActionCommand, InteractionRequestList,
    InteractionRequestReceipt, InteractionRequestSyncState, NotificationActionCommand, NotificationList,
    NotificationReceipt, NotificationStatus, NotificationSyncState, SetGeaAuthSessionRequest,
};
use aionui_db::{
    IConversationRepository, IGeaResourceRepository, IInteractionRequestRepository, INotificationRepository,
    ReceiptResumeClaim, StoredInteractionRequestReceipt,
};
use aionui_realtime::EventBroadcaster;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::GeaError;
use crate::interaction_request::{parse_receipt, parse_snapshot, validate_action_command, validate_question_answers};
use crate::notification::{parse_notification_receipt, parse_notification_snapshot, validate_notification_action};

#[path = "service/notification.rs"]
mod notification_service;
#[path = "service/interaction_request.rs"]
mod projection_service;
#[path = "service/resource_catalog.rs"]
mod resource_catalog_service;

use self::notification_service::NotificationProjection;
use self::projection_service::{InteractionRequestProjection, RESUME_CLAIM_LEASE_MS};
use crate::{InteractionTurnResolver, InteractionTurnResumer};

const DEFAULT_GEA_BASE_URL: &str = "https://gea.synear.cn:4443/gea-boot";
const GEA_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GEA_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const INTERACTION_POLL_MAX_BACKOFF: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const INTERACTION_RESUME_TIMEOUT_MS: u64 = 10_000;
#[cfg(test)]
const INTERACTION_RESUME_TIMEOUT_MS: u64 = 100;
// The callback is cancelled before a foreign process can take over its DB
// claim, so two owners cannot concurrently deliver the same receipt.
const _: () = assert!(INTERACTION_RESUME_TIMEOUT_MS < RESUME_CLAIM_LEASE_MS as u64);
const GEA_CONTEXT_FIELDS: &[&str] = &[
    "agentcode",
    "auditid",
    "authorizationrevision",
    "channel",
    "consumercode",
    "consumertype",
    "conversationid",
    "delegationtoken",
    "mcpcode",
    "principalid",
    "principaltype",
    "requestid",
    "sessionid",
    "tenantid",
    "toolname",
    "traceid",
    "userid",
];

type InteractionScope = (String, String);
type InteractionMutex = Arc<tokio::sync::Mutex<()>>;
type InteractionLockMap = Arc<tokio::sync::Mutex<HashMap<InteractionScope, InteractionMutex>>>;
type InteractionPollRegistry = Arc<tokio::sync::Mutex<HashMap<String, InteractionPollControl>>>;

#[derive(Clone)]
struct InteractionPollControl {
    id: Uuid,
    stop: tokio::sync::watch::Sender<bool>,
    wake: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
struct GeaCredential {
    access_token: Arc<str>,
    tenant_id: Option<String>,
}

#[derive(Clone)]
struct GeaConversationSession {
    agent_code: String,
    session_id: String,
    conversation_id: String,
    delegation_token: Arc<str>,
    tools: HashMap<String, GeaToolInfo>,
}

/// A per-process GEA gateway. Credentials and delegation tokens deliberately
/// remain private and are indexed by the authenticated AionCore user.
#[derive(Clone)]
pub struct GeaService {
    client: reqwest::Client,
    base_url: String,
    credentials: Arc<RwLock<HashMap<String, GeaCredential>>>,
    reauth_required: Arc<RwLock<HashSet<String>>>,
    sessions: Arc<RwLock<HashMap<(String, String), GeaConversationSession>>>,
    interaction_locks: InteractionLockMap,
    projection: Option<InteractionRequestProjection>,
    notification_projection: Option<NotificationProjection>,
    turn_resumer: Option<InteractionTurnResumer>,
    resume_claim_owner: Arc<str>,
    interaction_poll_interval: Option<Duration>,
    interaction_pollers: InteractionPollRegistry,
    resource_repo: Option<Arc<dyn IGeaResourceRepository>>,
    managed_skill_root: Option<Arc<PathBuf>>,
}

impl GeaService {
    pub fn from_env() -> Result<Self, GeaError> {
        let base_url = std::env::var("AIONUI_GEA_BASE_URL").unwrap_or_else(|_| DEFAULT_GEA_BASE_URL.to_owned());
        let client = reqwest::Client::builder()
            .connect_timeout(GEA_CONNECT_TIMEOUT)
            .timeout(GEA_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| GeaError::server_error("GEA_CLIENT_INIT_FAILED", "GEA 客户端初始化失败"))?;
        Self::new(client, base_url).map(|mut service| {
            service.interaction_poll_interval = Some(Duration::from_secs(3));
            service
        })
    }

    pub fn new(client: reqwest::Client, base_url: impl Into<String>) -> Result<Self, GeaError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        if !(base_url.starts_with("https://")
            || cfg!(any(test, feature = "test-support")) && base_url.starts_with("http://"))
        {
            return Err(GeaError::invalid_request("GEA 地址必须使用 HTTPS"));
        }
        Ok(Self {
            client,
            base_url,
            credentials: Arc::new(RwLock::new(HashMap::new())),
            reauth_required: Arc::new(RwLock::new(HashSet::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            interaction_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            projection: None,
            notification_projection: None,
            turn_resumer: None,
            resume_claim_owner: Arc::from(Uuid::now_v7().to_string()),
            interaction_poll_interval: None,
            interaction_pollers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            resource_repo: None,
            managed_skill_root: None,
        })
    }

    pub fn with_resource_catalog(
        mut self,
        resource_repo: Arc<dyn IGeaResourceRepository>,
        managed_skill_root: PathBuf,
    ) -> Self {
        self.resource_repo = Some(resource_repo);
        self.managed_skill_root = Some(Arc::new(managed_skill_root));
        self
    }

    pub fn with_interaction_request_projection(
        mut self,
        interaction_repo: Arc<dyn IInteractionRequestRepository>,
        conversation_repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        turn_resolver: Option<InteractionTurnResolver>,
    ) -> Self {
        self.projection = Some(InteractionRequestProjection::new(
            interaction_repo,
            conversation_repo,
            broadcaster,
            turn_resolver,
        ));
        self
    }

    pub fn with_notification_projection(
        mut self,
        notification_repo: Arc<dyn INotificationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
    ) -> Self {
        self.notification_projection = Some(NotificationProjection::new(notification_repo, broadcaster));
        self
    }

    pub fn with_interaction_turn_resumer(mut self, turn_resumer: InteractionTurnResumer) -> Self {
        self.turn_resumer = Some(turn_resumer);
        self
    }

    pub async fn set_auth_session(
        &self,
        user_id: &str,
        request: SetGeaAuthSessionRequest,
    ) -> Result<GeaAuthSessionStatus, GeaError> {
        let access_token = request.access_token.trim();
        if access_token.is_empty() {
            return Err(GeaError::invalid_request("GEA access token 不能为空"));
        }
        let tenant_id = request.tenant_id.and_then(non_empty);
        if let Some(repo) = &self.resource_repo {
            repo.set_active_scope(user_id, tenant_id.as_deref().unwrap_or(""), &self.base_url)
                .await
                .map_err(|error| {
                    tracing::error!(error = %error, user_id, "failed to activate GEA Resource Catalog scope");
                    GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存不可用")
                })?;
        }
        self.stop_interaction_request_poll(user_id).await;
        self.credentials.write().await.insert(
            user_id.to_owned(),
            GeaCredential {
                access_token: Arc::from(access_token),
                tenant_id: tenant_id.clone(),
            },
        );
        self.reauth_required.write().await.remove(user_id);
        self.clear_sessions(user_id).await;
        self.recover_interaction_sessions(user_id).await;
        self.ensure_interaction_request_poll(user_id);
        if self.projection.is_some() {
            self.recover_unfinalized_receipts(user_id).await?;
        }
        Ok(GeaAuthSessionStatus {
            authenticated: true,
            reauth_required: false,
            tenant_id,
        })
    }

    pub async fn auth_status(&self, user_id: &str) -> GeaAuthSessionStatus {
        let credential = self.credentials.read().await.get(user_id).cloned();
        match credential {
            Some(value) => GeaAuthSessionStatus {
                authenticated: true,
                reauth_required: false,
                tenant_id: value.tenant_id,
            },
            None => GeaAuthSessionStatus {
                authenticated: false,
                reauth_required: self.reauth_required.read().await.contains(user_id),
                tenant_id: None,
            },
        }
    }

    pub async fn clear_auth_session(&self, user_id: &str) {
        self.stop_interaction_request_poll(user_id).await;
        self.credentials.write().await.remove(user_id);
        self.reauth_required.write().await.remove(user_id);
        self.clear_sessions(user_id).await;
        self.clear_resource_scope(user_id).await;
    }

    async fn invalidate_auth_session(&self, user_id: &str) {
        self.stop_interaction_request_poll(user_id).await;
        self.credentials.write().await.remove(user_id);
        self.reauth_required.write().await.insert(user_id.to_owned());
        self.clear_sessions(user_id).await;
        self.clear_resource_scope(user_id).await;
    }

    async fn clear_resource_scope(&self, user_id: &str) {
        if let Some(repo) = &self.resource_repo
            && let Err(error) = repo.clear_active_scope(user_id).await
        {
            tracing::error!(error = %error, user_id, "failed to clear GEA Resource Catalog scope");
        }
    }

    pub async fn create_session(
        &self,
        user_id: &str,
        conversation_id: &str,
        request: CreateGeaSessionRequest,
    ) -> Result<GeaSessionResponse, GeaError> {
        self.create_session_inner(user_id, conversation_id, request, true).await
    }

    async fn create_session_inner(
        &self,
        user_id: &str,
        conversation_id: &str,
        request: CreateGeaSessionRequest,
        persist_for_conversation: bool,
    ) -> Result<GeaSessionResponse, GeaError> {
        let consumer_code = request.consumer_code.trim();
        if consumer_code.is_empty() || conversation_id.trim().is_empty() {
            return Err(GeaError::invalid_request("consumerCode 和 conversationId 不能为空"));
        }
        let credential = self.credential(user_id).await?;
        let request_id = Uuid::now_v7().to_string();
        let mut body = json!({
            "consumerType": "AGENT",
            "consumerCode": consumer_code,
            "requestId": request_id,
            "conversationId": conversation_id,
            "channel": "AION_CORE"
        });
        let preparation_id = request.preparation_id.and_then(non_empty);
        if let Some(preparation_id) = preparation_id.as_ref() {
            body["preparationId"] = Value::String(preparation_id.clone());
        }

        let (value, legacy_session) = match self
            .post_for_user(user_id, &credential, "/ai/gateway/session", &body)
            .await
        {
            Ok(value) => (value, false),
            Err(error) if error.body.code == "404" => {
                tracing::warn!(
                    user_id,
                    conversation_id,
                    consumer_code,
                    "GEA unified session endpoint is unavailable; falling back to the deployed agent session endpoint"
                );
                let legacy_body = json!({
                    "agentCode": consumer_code,
                    "channel": "CS_CLIENT"
                });
                (
                    self.post_for_user(user_id, &credential, "/ai/gateway/agent/session", &legacy_body)
                        .await?,
                    true,
                )
            }
            Err(error) => return Err(error),
        };
        let result = value
            .get("result")
            .ok_or_else(|| invalid_upstream("GEA Session 响应缺少 result"))?;
        let allowed = result
            .pointer("/accessDecision/allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allowed {
            return Err(access_denied_error(&value));
        }
        let context = result
            .get("gatewayContext")
            .ok_or_else(|| invalid_upstream("GEA Session 响应缺少 gatewayContext"))?;
        let session_id = required_string(context, "sessionId")?;
        let returned_conversation_id = required_string(context, "conversationId")?;
        if !legacy_session && returned_conversation_id != conversation_id {
            return Err(invalid_upstream("GEA Session 返回了不匹配的 conversationId"));
        }
        let returned_agent_code = context
            .get("agentId")
            .or_else(|| context.get("consumerCode"))
            .and_then(Value::as_str)
            .unwrap_or(consumer_code);
        if returned_agent_code != consumer_code {
            return Err(invalid_upstream("GEA Session 返回了不匹配的 consumerCode"));
        }
        let delegation_token = required_string(result, "delegationToken")?;
        let effective_capability_codes = result
            .get("effectiveCapabilityCodes")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();

        let interaction_lock = self.interaction_lock(user_id, conversation_id).await;
        let _interaction_guard = interaction_lock.lock().await;
        self.sessions.write().await.insert(
            (user_id.to_owned(), conversation_id.to_owned()),
            GeaConversationSession {
                agent_code: consumer_code.to_owned(),
                session_id: session_id.clone(),
                conversation_id: returned_conversation_id.clone(),
                delegation_token: Arc::from(delegation_token),
                tools: HashMap::new(),
            },
        );
        if persist_for_conversation && let Some(projection) = self.projection.as_ref() {
            projection
                .store_session_bootstrap(user_id, conversation_id, consumer_code, preparation_id)
                .await?;
        }
        if persist_for_conversation {
            self.ensure_interaction_request_poll(user_id);
        }
        tracing::info!(
            user_id,
            conversation_id,
            consumer_code,
            request_id,
            "GEA gateway session created"
        );
        Ok(GeaSessionResponse {
            session_id,
            conversation_id: returned_conversation_id,
            consumer_code: consumer_code.to_owned(),
            effective_capability_codes,
        })
    }

    pub async fn test_mcp_connection(
        &self,
        user_id: &str,
        consumer_code: String,
    ) -> Result<Vec<GeaToolInfo>, GeaError> {
        let conversation_id = format!("gea-mcp-test-{}", Uuid::now_v7());
        self.test_mcp_connection_with_id(user_id, consumer_code, conversation_id)
            .await
    }

    async fn test_mcp_connection_with_id(
        &self,
        user_id: &str,
        consumer_code: String,
        conversation_id: String,
    ) -> Result<Vec<GeaToolInfo>, GeaError> {
        self.create_session_inner(
            user_id,
            &conversation_id,
            CreateGeaSessionRequest {
                consumer_code,
                preparation_id: None,
            },
            false,
        )
        .await?;
        let result = self.list_tools(user_id, &conversation_id).await;
        self.sessions
            .write()
            .await
            .remove(&(user_id.to_owned(), conversation_id));
        result
    }

    pub async fn list_tools(&self, user_id: &str, conversation_id: &str) -> Result<Vec<GeaToolInfo>, GeaError> {
        let credential = self.credential(user_id).await?;
        let session = self.session(user_id, conversation_id).await?;
        let body = session.gateway_body();
        let value = self
            .post_for_conversation(
                user_id,
                conversation_id,
                &credential,
                "/ai/gateway/mcp/proxy/list",
                &body,
            )
            .await?;
        let raw_tools = value
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_upstream("GEA Tool 列表响应缺少 tools"))?;
        let mut tools = Vec::with_capacity(raw_tools.len());
        let mut names = HashSet::with_capacity(raw_tools.len());
        for raw in raw_tools {
            let name = required_string(raw, "name")?;
            if !names.insert(name.clone()) {
                return Err(invalid_upstream("GEA Tool 列表存在重名工具"));
            }
            let source_code = required_string(raw, "sourceCode")?;
            let input_schema = sanitize_tool_input_schema(
                raw.get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            )?;
            tools.push(GeaToolInfo {
                name,
                source_code,
                description: raw
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_schema,
            });
        }
        let tools_by_name = tools.iter().cloned().map(|tool| (tool.name.clone(), tool)).collect();
        if let Some(stored) = self
            .sessions
            .write()
            .await
            .get_mut(&(user_id.to_owned(), conversation_id.to_owned()))
        {
            stored.tools = tools_by_name;
        }
        Ok(tools)
    }

    pub async fn call_tool(
        &self,
        user_id: &str,
        conversation_id: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<GeaToolCallResponse, GeaError> {
        if !arguments.is_object() && !arguments.is_null() {
            return Err(GeaError::invalid_request("arguments 必须是 JSON object"));
        }
        let credential = self.credential(user_id).await?;
        let mut session = self.session(user_id, conversation_id).await?;
        if !session.tools.contains_key(tool_name) {
            self.list_tools(user_id, conversation_id).await?;
            session = self.session(user_id, conversation_id).await?;
        }
        let tool = session
            .tools
            .get(tool_name)
            .cloned()
            .ok_or_else(|| GeaError::tool_not_found(tool_name))?;
        let body = json!({
            "agentCode": session.agent_code,
            "sessionId": session.session_id,
            "conversationId": session.conversation_id,
            "delegationToken": session.delegation_token.as_ref(),
            "mcpCode": tool.source_code,
            "toolName": tool.name,
            "arguments": if arguments.is_null() { json!({}) } else { arguments }
        });
        let started = Instant::now();
        let value = self
            .post_for_conversation(
                user_id,
                conversation_id,
                &credential,
                "/ai/gateway/mcp/proxy/call",
                &body,
            )
            .await?;
        if value.get("sourceCode").and_then(Value::as_str) != Some(tool.source_code.as_str())
            || value.get("toolName").and_then(Value::as_str) != Some(tool.name.as_str())
        {
            return Err(invalid_upstream("GEA Tool 调用响应与请求不匹配"));
        }
        let audit_id = value.get("auditId").and_then(Value::as_str).and_then(non_empty);
        tracing::info!(
            user_id,
            conversation_id,
            tool_name,
            audit_id,
            duration_ms = started.elapsed().as_millis(),
            "GEA tool call completed"
        );
        Ok(GeaToolCallResponse {
            result: value.get("result").cloned().unwrap_or(Value::Null),
            audit_id,
        })
    }

    pub async fn list_interaction_requests(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<GeaInteractionRequestSnapshot, GeaError> {
        let interaction_lock = self.interaction_lock(user_id, conversation_id).await;
        let _interaction_guard = interaction_lock.lock().await;
        let credential = self.credential(user_id).await?;
        let session = self.session(user_id, conversation_id).await?;
        self.list_interaction_requests_unlocked(user_id, conversation_id, &credential, &session)
            .await
    }

    async fn list_interaction_requests_unlocked(
        &self,
        user_id: &str,
        conversation_id: &str,
        credential: &GeaCredential,
        session: &GeaConversationSession,
    ) -> Result<GeaInteractionRequestSnapshot, GeaError> {
        let started = Instant::now();
        let value = self
            .get_for_conversation(user_id, conversation_id, credential, session)
            .await?;
        let snapshot = parse_snapshot(&value)?;
        let mut projection_changed = None;
        if let Some(projection) = &self.projection {
            projection_changed = Some(
                projection
                    .reconcile_snapshot(user_id, conversation_id, &snapshot)
                    .await?,
            );
        }
        if projection_changed != Some(false) {
            tracing::info!(
                user_id,
                conversation_id,
                revision = snapshot.revision,
                snapshot_count = snapshot.items.len(),
                duration_ms = started.elapsed().as_millis(),
                "GEA interaction request snapshot changed"
            );
        } else {
            tracing::debug!(
                user_id,
                conversation_id,
                revision = snapshot.revision,
                snapshot_count = snapshot.items.len(),
                duration_ms = started.elapsed().as_millis(),
                "GEA interaction request snapshot unchanged"
            );
        }
        Ok(snapshot)
    }

    pub async fn list_all_interaction_requests(&self, user_id: &str) -> Result<InteractionRequestList, GeaError> {
        let projection = self.projection()?;
        let has_credential = self.credentials.read().await.contains_key(user_id);
        let mut failures = self.recover_interaction_sessions(user_id).await;
        self.recover_unfinalized_receipts(user_id).await?;
        let mut conversation_ids = self
            .sessions
            .read()
            .await
            .keys()
            .filter(|(owner, _)| owner == user_id)
            .map(|(_, conversation_id)| conversation_id.clone())
            .collect::<Vec<_>>();
        conversation_ids.sort();
        let mut synchronized = false;
        for conversation_id in conversation_ids {
            let started = Instant::now();
            match self.list_interaction_requests(user_id, &conversation_id).await {
                Ok(_) => {
                    synchronized = true;
                    break;
                }
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        conversation_id,
                        code = %error.body.code,
                        duration_ms = started.elapsed().as_millis(),
                        "GEA interaction request refresh failed"
                    );
                    failures.push((conversation_id, error.body.code.clone()));
                }
            }
        }
        let mut list = projection.list_active(user_id).await?;
        if synchronized {
            return Ok(list);
        }
        if failures.is_empty() {
            let code = if has_credential {
                "GEA_SESSION_REQUIRED"
            } else {
                "GEA_AUTH_REQUIRED"
            };
            failures.push((String::new(), code.to_owned()));
        }
        list.sync_state = InteractionRequestSyncState::Failed;
        list.failed_session_count = failures.len();
        list.failure_codes = failures
            .iter()
            .map(|(_, code)| code.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        list.failure_codes.sort();
        for item in &mut list.items {
            item.stale = true;
        }
        Ok(list)
    }

    pub async fn list_notifications(&self, user_id: &str, status: Option<&str>) -> Result<NotificationList, GeaError> {
        let started = Instant::now();
        let trace_id = Uuid::now_v7().to_string();
        let credential = self.credential(user_id).await?;
        let tenant_id = credential.tenant_id.as_deref().unwrap_or("");
        let projection = self.notification_projection()?;
        let sync_lock = projection.sync_lock(user_id, tenant_id).await;
        let _sync_guard = match sync_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                let mut list = projection.list(user_id, tenant_id, status).await?;
                list.sync_state = NotificationSyncState::Syncing;
                list.failure_codes.clear();
                return Ok(list);
            }
        };
        projection
            .set_sync_state(user_id, tenant_id, NotificationSyncState::Syncing, Vec::new())
            .await;
        tracing::info!(
            event = "notification.sync.started",
            trace_id,
            trigger = "client_or_poll",
            attempt = 1,
            "GEA Notification sync started"
        );
        let result = self.fetch_notification_snapshot(user_id, &credential, &trace_id).await;
        match result {
            Ok(snapshot) => {
                let changed = projection
                    .reconcile_snapshot(user_id, tenant_id, &snapshot, &trace_id)
                    .await?;
                projection
                    .set_sync_state(user_id, tenant_id, NotificationSyncState::Fresh, Vec::new())
                    .await;
                let list = projection.list(user_id, tenant_id, status).await?;
                tracing::info!(
                    trace_id,
                    revision = snapshot.revision,
                    item_count = snapshot.items.len(),
                    changed,
                    duration_ms = started.elapsed().as_millis(),
                    event = "notification.sync.succeeded",
                    attempt = 1,
                    sync_state = "fresh",
                    result = "succeeded",
                    "GEA Notification sync succeeded"
                );
                Ok(list)
            }
            Err((error, partial_page_count)) => {
                let mut list = projection.list(user_id, tenant_id, status).await?;
                list.sync_state = if partial_page_count > 0 && !list.revision.is_empty() {
                    NotificationSyncState::Partial
                } else if list.revision.is_empty() {
                    NotificationSyncState::Failed
                } else {
                    NotificationSyncState::Stale
                };
                list.failure_codes = vec![error.body.code.clone()];
                projection
                    .set_sync_state(user_id, tenant_id, list.sync_state, list.failure_codes.clone())
                    .await;
                tracing::warn!(
                    trace_id,
                    code = %error.body.code,
                    category = %error.body.category,
                    retryable = error.body.retryable,
                    partial_page_count,
                    duration_ms = started.elapsed().as_millis(),
                    event = "notification.sync.failed",
                    attempt = 1,
                    sync_state = ?list.sync_state,
                    result = "preserved_last_good",
                    "GEA Notification sync failed; preserving last-good projection"
                );
                Ok(list)
            }
        }
    }

    async fn fetch_notification_snapshot(
        &self,
        user_id: &str,
        credential: &GeaCredential,
        trace_id: &str,
    ) -> Result<GeaNotificationSnapshot, (GeaError, usize)> {
        const PAGE_LIMIT: usize = 200;
        const MAX_PAGES: usize = 100;
        let mut revision: Option<String> = None;
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut seen_ids = HashSet::new();
        let mut items = Vec::new();
        let mut page_count = 0;
        loop {
            let path = match cursor.as_deref() {
                Some(cursor) => format!(
                    "/ai/gateway/notifications?limit={PAGE_LIMIT}&cursor={}",
                    encode_path_segment(cursor)
                ),
                None => format!("/ai/gateway/notifications?limit={PAGE_LIMIT}"),
            };
            let value = self
                .get_for_user_path(user_id, credential, &path, trace_id)
                .await
                .map_err(|error| (error, page_count))?;
            let page = parse_notification_snapshot(&value).map_err(|error| (error, page_count))?;
            page_count += 1;
            if revision.as_deref().is_some_and(|current| current != page.revision) {
                return Err((
                    GeaError::bad_gateway("GEA_INVALID_RESPONSE", "GEA Notification 分页 revision 不一致"),
                    page_count,
                ));
            }
            revision.get_or_insert_with(|| page.revision.clone());
            for item in page.items {
                if !seen_ids.insert(item.id.clone()) {
                    return Err((
                        GeaError::bad_gateway("GEA_INVALID_RESPONSE", "GEA Notification 分页包含重复 notification ID"),
                        page_count,
                    ));
                }
                items.push(item);
            }
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            if page_count >= MAX_PAGES || !seen_cursors.insert(next_cursor.clone()) {
                return Err((
                    GeaError::bad_gateway("GEA_INVALID_RESPONSE", "GEA Notification 分页 cursor 无效"),
                    page_count,
                ));
            }
            cursor = Some(next_cursor);
        }
        Ok(GeaNotificationSnapshot {
            revision: revision.unwrap_or_default(),
            items,
            next_cursor: None,
        })
    }

    pub async fn get_notification(
        &self,
        user_id: &str,
        notification_id: &str,
    ) -> Result<aionui_api_types::NotificationView, GeaError> {
        let credential = self.credential(user_id).await?;
        self.notification_projection()?
            .find(user_id, credential.tenant_id.as_deref().unwrap_or(""), notification_id)
            .await
    }

    pub async fn mark_notification_read(
        &self,
        user_id: &str,
        notification_id: &str,
        command: NotificationActionCommand,
    ) -> Result<NotificationReceipt, GeaError> {
        self.act_on_notification(user_id, notification_id, "read", command)
            .await
    }

    pub async fn dismiss_notification(
        &self,
        user_id: &str,
        notification_id: &str,
        command: NotificationActionCommand,
    ) -> Result<NotificationReceipt, GeaError> {
        self.act_on_notification(user_id, notification_id, "dismiss", command)
            .await
    }

    async fn act_on_notification(
        &self,
        user_id: &str,
        notification_id: &str,
        action: &str,
        command: NotificationActionCommand,
    ) -> Result<NotificationReceipt, GeaError> {
        let started = Instant::now();
        let trace_id = Uuid::now_v7().to_string();
        let idempotency_key_hash = hex::encode(Sha256::digest(command.idempotency_key.as_bytes()));
        tracing::info!(
            event = "notification.action.started",
            trace_id,
            notification_id,
            action,
            expected_version = %command.expected_version,
            idempotency_key_hash,
            "GEA Notification action started"
        );
        let result = self
            .act_on_notification_inner(user_id, notification_id, action, &command, &trace_id)
            .await;
        match &result {
            Ok(receipt) => tracing::info!(
                event = "notification.action.completed",
                trace_id,
                notification_id,
                action,
                expected_version = %command.expected_version,
                idempotency_key_hash,
                version = %receipt.version,
                duration_ms = started.elapsed().as_millis(),
                "GEA Notification action completed"
            ),
            Err(error) if error.status == axum::http::StatusCode::CONFLICT => tracing::warn!(
                event = "notification.action.conflicted",
                trace_id,
                notification_id,
                action,
                expected_version = %command.expected_version,
                idempotency_key_hash,
                code = %error.body.code,
                category = %error.body.category,
                retryable = error.body.retryable,
                duration_ms = started.elapsed().as_millis(),
                "GEA Notification action conflicted"
            ),
            Err(error) => tracing::warn!(
                event = "notification.action.failed",
                trace_id,
                notification_id,
                action,
                expected_version = %command.expected_version,
                idempotency_key_hash,
                code = %error.body.code,
                category = %error.body.category,
                retryable = error.body.retryable,
                duration_ms = started.elapsed().as_millis(),
                "GEA Notification action failed"
            ),
        }
        result
    }

    async fn act_on_notification_inner(
        &self,
        user_id: &str,
        notification_id: &str,
        action: &str,
        command: &NotificationActionCommand,
        trace_id: &str,
    ) -> Result<NotificationReceipt, GeaError> {
        validate_notification_action(notification_id, command)?;
        let credential = self.credential(user_id).await?;
        let tenant_id = credential.tenant_id.as_deref().unwrap_or("");
        let projection = self.notification_projection()?;
        let action_lock = projection.action_lock(user_id, tenant_id, notification_id).await;
        let _guard = action_lock.lock().await;
        let mutation_gate = projection.mutation_gate(user_id, tenant_id).await;
        let _mutation_guard = mutation_gate.read().await;

        if let Some(receipt) = projection
            .load_receipt(
                user_id,
                tenant_id,
                notification_id,
                &command.idempotency_key,
                &command.expected_version,
                action,
            )
            .await?
        {
            return Ok(receipt);
        }
        if let Some(receipt) = projection
            .load_equivalent_receipt(user_id, tenant_id, notification_id, &command.expected_version, action)
            .await?
        {
            return Ok(receipt);
        }

        let current = projection.find(user_id, tenant_id, notification_id).await?;
        let state_details = || {
            json!({
                "notificationId": current.id,
                "version": current.version,
                "status": current.status,
                "dismissible": current.dismissible,
            })
        };
        if current.version != command.expected_version {
            let mut error = GeaError::conflict(
                "GEA_NOTIFICATION_VERSION_CONFLICT",
                "Notification 版本已变化，请刷新后重试",
            );
            error.body.details = Some(state_details());
            return Err(error);
        }
        if action == "dismiss" && !current.dismissible {
            let mut error = GeaError::conflict("GEA_NOTIFICATION_NOT_DISMISSIBLE", "该 Notification 不允许关闭");
            error.body.details = Some(state_details());
            return Err(error);
        }
        if current.expires_at.as_deref().is_some_and(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .ok()
                .is_some_and(|value| value <= chrono::Utc::now())
        }) {
            let mut error = GeaError::conflict("GEA_NOTIFICATION_EXPIRED", "该 Notification 已过期");
            error.body.details = Some(state_details());
            return Err(error);
        }
        if current.status == NotificationStatus::Dismissed {
            let mut error = GeaError::conflict("GEA_NOTIFICATION_ALREADY_DISMISSED", "该 Notification 已关闭");
            error.body.details = Some(state_details());
            return Err(error);
        }
        if action == "read" && current.status == NotificationStatus::Read {
            let mut error = GeaError::conflict("GEA_NOTIFICATION_ALREADY_READ", "该 Notification 已读");
            error.body.details = Some(state_details());
            return Err(error);
        }
        let path = format!(
            "/ai/gateway/notifications/{}/{}",
            encode_path_segment(notification_id.trim()),
            action
        );
        let value = self
            .post_for_user_with_trace(
                user_id,
                &credential,
                &path,
                &json!({
                    "expectedVersion": command.expected_version,
                    "idempotencyKey": command.idempotency_key,
                }),
                trace_id,
            )
            .await?;
        let upstream = parse_notification_receipt(&value, notification_id)?;
        let expected_status = if action == "dismiss" {
            NotificationStatus::Dismissed
        } else {
            NotificationStatus::Read
        };
        if upstream.status != expected_status {
            return Err(invalid_upstream("GEA Notification 回执状态与动作不匹配"));
        }
        let receipt = NotificationReceipt {
            receipt_id: upstream.receipt_id,
            notification_id: upstream.notification_id,
            version: upstream.version,
            status: upstream.status,
            notification: None,
        };
        projection
            .store_receipt(
                user_id,
                tenant_id,
                notification_id,
                &command.expected_version,
                &command.idempotency_key,
                action,
                &receipt,
                trace_id,
            )
            .await?;
        Ok(receipt)
    }

    async fn recover_unfinalized_receipts(&self, user_id: &str) -> Result<(), GeaError> {
        let projection = self.projection()?;
        for stored in projection.list_unfinalized_receipts(user_id).await? {
            let request_id = stored.request_id;
            let idempotency_key = stored.idempotency_key;
            let receipt = match serde_json::from_str::<InteractionRequestReceipt>(&stored.receipt) {
                Ok(receipt) => receipt,
                Err(error) => {
                    tracing::error!(
                        user_id,
                        request_id,
                        error = %error,
                        "stored unfinalized Interaction Request receipt is invalid"
                    );
                    continue;
                }
            };
            let action_lock = projection.action_lock(user_id, &request_id).await;
            let _guard = action_lock.lock().await;
            if let Err(error) = self
                .finalize_interaction_receipt(user_id, &request_id, &idempotency_key, &receipt)
                .await
            {
                tracing::warn!(
                    user_id,
                    request_id,
                    code = %error.body.code,
                    "unfinalized Interaction Request receipt recovery is still pending"
                );
            }
        }
        Ok(())
    }

    async fn recover_interaction_sessions(&self, user_id: &str) -> Vec<(String, String)> {
        let mut failures = Vec::new();
        if !self.credentials.read().await.contains_key(user_id) {
            return failures;
        }
        if self.sessions.read().await.keys().any(|(owner, _)| owner == user_id) {
            return failures;
        }
        let Some(projection) = self.projection.as_ref() else {
            return failures;
        };
        let bootstraps = match projection.session_bootstraps(user_id).await {
            Ok(bootstraps) => bootstraps,
            Err(error) => {
                tracing::warn!(user_id, code = %error.body.code, "could not load GEA session recovery records");
                failures.push((String::new(), error.body.code.clone()));
                return failures;
            }
        };
        for bootstrap in bootstraps {
            let key = (user_id.to_owned(), bootstrap.conversation_id.clone());
            if self.sessions.read().await.contains_key(&key) {
                continue;
            }
            match self
                .create_session(
                    user_id,
                    &bootstrap.conversation_id,
                    CreateGeaSessionRequest {
                        consumer_code: bootstrap.consumer_code,
                        preparation_id: bootstrap.preparation_id,
                    },
                )
                .await
            {
                Ok(_) => break,
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        conversation_id = %bootstrap.conversation_id,
                        code = %error.body.code,
                        "GEA session recovery failed; keeping the durable Interaction Request projection"
                    );
                    failures.push((bootstrap.conversation_id, error.body.code.clone()));
                }
            }
        }
        failures
    }

    pub async fn act_on_global_interaction_request(
        &self,
        user_id: &str,
        request_id: &str,
        command: InteractionRequestActionCommand,
    ) -> Result<InteractionRequestReceipt, GeaError> {
        let gea_command = GeaInteractionRequestActionCommand {
            expected_version: command.expected_version.clone(),
            idempotency_key: command.idempotency_key.clone(),
            action_id: command.action_id.clone(),
            payload: command.payload.clone(),
        };
        validate_action_command(request_id, &gea_command)?;
        let projection = self.projection()?;
        let action_lock = projection.action_lock(user_id, request_id).await;
        let _guard = action_lock.lock().await;
        if let Some(stored) = projection
            .load_receipt(user_id, request_id, &command.idempotency_key)
            .await?
        {
            if stored.expected_version != command.expected_version || stored.action_id != command.action_id {
                return Err(GeaError::invalid_request("同一 idempotencyKey 不能用于不同版本或动作"));
            }
            let receipt = serde_json::from_str(&stored.receipt).map_err(|error| {
                tracing::error!(error = %error, request_id, "stored Interaction Request receipt is invalid");
                GeaError::internal("Interaction Request 回执无效")
            })?;
            return self.finalize_stored_receipt(user_id, request_id, stored, receipt).await;
        }
        if let Some(stored) = projection
            .load_equivalent_receipt(user_id, request_id, &command.expected_version, &command.action_id)
            .await?
        {
            let receipt = serde_json::from_str(&stored.receipt).map_err(|error| {
                tracing::error!(error = %error, request_id, "stored equivalent Interaction Request receipt is invalid");
                GeaError::internal("Interaction Request 回执无效")
            })?;
            return self.finalize_stored_receipt(user_id, request_id, stored, receipt).await;
        }

        let current = projection.find(user_id, request_id).await?;
        let conversation_id = {
            let sessions = self.sessions.read().await;
            if sessions.contains_key(&(user_id.to_owned(), current.conversation_id.clone())) {
                current.conversation_id.clone()
            } else {
                let mut conversation_ids = sessions
                    .keys()
                    .filter(|(owner, _)| owner == user_id)
                    .map(|(_, conversation_id)| conversation_id.clone())
                    .collect::<Vec<_>>();
                conversation_ids.sort();
                conversation_ids
                    .into_iter()
                    .next()
                    .ok_or_else(|| GeaError::conflict("GEA_SESSION_REQUIRED", "待办操作需要先恢复有效的 GEA Session"))?
            }
        };
        let interaction_lock = self.interaction_lock(user_id, &conversation_id).await;
        let _interaction_guard = interaction_lock.lock().await;
        let credential = self.credential(user_id).await?;
        let session = self.session(user_id, &conversation_id).await?;
        let upstream = self
            .act_on_interaction_request_unlocked(
                user_id,
                &conversation_id,
                request_id,
                gea_command,
                &credential,
                &session,
            )
            .await?;
        let needs_authoritative_refresh = upstream.request.is_none()
            && matches!(
                upstream.status,
                GeaInteractionRequestReceiptStatus::Conflict
                    | GeaInteractionRequestReceiptStatus::Forbidden
                    | GeaInteractionRequestReceiptStatus::Expired
                    | GeaInteractionRequestReceiptStatus::UnknownExternalWrite
            );
        let mut request = if let Some(authoritative) = upstream.request.as_ref() {
            Some(projection.apply_authoritative_request(user_id, authoritative).await?)
        } else if needs_authoritative_refresh {
            match self
                .list_interaction_requests_unlocked(user_id, &conversation_id, &credential, &session)
                .await
            {
                Ok(_) => projection
                    .find(user_id, request_id)
                    .await
                    .ok()
                    .or_else(|| Some(current.clone())),
                Err(error) => {
                    tracing::warn!(
                        user_id,
                        request_id,
                        code = %error.body.code,
                        "authoritative refresh failed after GEA returned a durable action receipt"
                    );
                    Some(current.clone())
                }
            }
        } else {
            Some(current)
        };
        if upstream.request.is_none()
            && let Some(request) = request.as_mut()
        {
            request.version = upstream.version.clone();
            request.status = match upstream.status {
                GeaInteractionRequestReceiptStatus::Accepted | GeaInteractionRequestReceiptStatus::AlreadyResolved => {
                    aionui_api_types::GeaInteractionRequestStatus::Resolved
                }
                GeaInteractionRequestReceiptStatus::Processing => {
                    aionui_api_types::GeaInteractionRequestStatus::Processing
                }
                GeaInteractionRequestReceiptStatus::Failed => aionui_api_types::GeaInteractionRequestStatus::Pending,
                GeaInteractionRequestReceiptStatus::Expired => aionui_api_types::GeaInteractionRequestStatus::Expired,
                GeaInteractionRequestReceiptStatus::Cancelled => {
                    aionui_api_types::GeaInteractionRequestStatus::Cancelled
                }
                GeaInteractionRequestReceiptStatus::UnknownExternalWrite => {
                    aionui_api_types::GeaInteractionRequestStatus::VerificationRequired
                }
                GeaInteractionRequestReceiptStatus::Conflict | GeaInteractionRequestReceiptStatus::Forbidden => {
                    aionui_api_types::GeaInteractionRequestStatus::Pending
                }
            };
        }
        let receipt = InteractionRequestReceipt {
            receipt_id: upstream.receipt_id,
            request_id: upstream.request_id,
            version: upstream.version,
            status: upstream.status,
            turn_continuation: upstream.turn_continuation,
            resolved_at: upstream.resolved_at,
            resolved_by: upstream.resolved_by,
            request,
        };
        projection
            .store_receipt(
                user_id,
                request_id,
                &command.idempotency_key,
                &command.expected_version,
                &command.action_id,
                &receipt,
            )
            .await?;
        self.finalize_interaction_receipt(user_id, request_id, &command.idempotency_key, &receipt)
            .await?;
        Ok(receipt)
    }

    async fn finalize_stored_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        stored: StoredInteractionRequestReceipt,
        receipt: InteractionRequestReceipt,
    ) -> Result<InteractionRequestReceipt, GeaError> {
        if stored.finalized_at.is_none() {
            self.finalize_interaction_receipt(user_id, request_id, &stored.idempotency_key, &receipt)
                .await?;
        }
        Ok(receipt)
    }

    async fn finalize_interaction_receipt(
        &self,
        user_id: &str,
        request_id: &str,
        idempotency_key: &str,
        receipt: &InteractionRequestReceipt,
    ) -> Result<(), GeaError> {
        let require_resume_delivered = matches!(
            receipt.status,
            GeaInteractionRequestReceiptStatus::Accepted | GeaInteractionRequestReceiptStatus::AlreadyResolved
        );
        if require_resume_delivered {
            let projection = self.projection()?;
            let claim = projection
                .claim_receipt_resume(user_id, request_id, idempotency_key, &self.resume_claim_owner)
                .await?;
            match claim {
                ReceiptResumeClaim::Acquired => {
                    projection
                        .mark_receipt_resume_started(user_id, request_id, idempotency_key, &self.resume_claim_owner)
                        .await?;
                    let resume_result = match tokio::time::timeout(
                        Duration::from_millis(INTERACTION_RESUME_TIMEOUT_MS),
                        self.resume_interaction_turn(user_id, receipt),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(resume_delivery_unknown()),
                    };
                    if let Err(error) = resume_result {
                        tracing::warn!(
                            user_id,
                            request_id,
                            code = %error.body.code,
                            "Interaction Request Turn delivery outcome is unknown; automatic replay is disabled"
                        );
                        return Err(resume_delivery_unknown());
                    }
                    projection
                        .mark_receipt_resume_delivered(user_id, request_id, idempotency_key, &self.resume_claim_owner)
                        .await?;
                }
                ReceiptResumeClaim::Delivered => {}
                ReceiptResumeClaim::Unknown => return Err(resume_delivery_unknown()),
                ReceiptResumeClaim::Busy => {
                    return Err(GeaError::conflict(
                        "GEA_INTERACTION_RESUME_IN_PROGRESS",
                        "待办结果正在恢复原 Turn，请稍后重试",
                    ));
                }
            }
        }
        self.projection()?
            .finalize_receipt(user_id, request_id, idempotency_key, receipt, require_resume_delivered)
            .await
    }

    async fn resume_interaction_turn(
        &self,
        user_id: &str,
        receipt: &InteractionRequestReceipt,
    ) -> Result<(), GeaError> {
        let Some(request) = receipt.request.as_ref() else {
            return Err(GeaError::internal("Interaction Request 成功回执缺少本地关联"));
        };
        let Some(turn_id) = request.turn_id.as_ref() else {
            return Err(GeaError::conflict(
                "GEA_INTERACTION_TURN_UNAVAILABLE",
                "原 Turn 已不可恢复，请回到原会话重试",
            ));
        };
        let Some(resumer) = self.turn_resumer.as_ref() else {
            return Err(GeaError::internal("Interaction Request Turn 恢复通道未配置"));
        };
        resumer(
            user_id.to_owned(),
            request.conversation_id.clone(),
            turn_id.clone(),
            receipt.clone(),
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                user_id,
                request_id = %receipt.request_id,
                conversation_id = %request.conversation_id,
                turn_id,
                error,
                "accepted Interaction Request could not resume its original turn"
            );
            GeaError::conflict(
                "GEA_INTERACTION_TURN_UNAVAILABLE",
                "原 Turn 已不可恢复，请回到原会话重试",
            )
        })
    }

    pub async fn act_on_interaction_request(
        &self,
        user_id: &str,
        conversation_id: &str,
        request_id: &str,
        command: GeaInteractionRequestActionCommand,
    ) -> Result<GeaInteractionRequestReceipt, GeaError> {
        validate_action_command(request_id, &command)?;
        let interaction_lock = self.interaction_lock(user_id, conversation_id).await;
        let _interaction_guard = interaction_lock.lock().await;
        let credential = self.credential(user_id).await?;
        let session = self.session(user_id, conversation_id).await?;
        self.act_on_interaction_request_unlocked(user_id, conversation_id, request_id, command, &credential, &session)
            .await
    }

    async fn act_on_interaction_request_unlocked(
        &self,
        user_id: &str,
        conversation_id: &str,
        request_id: &str,
        command: GeaInteractionRequestActionCommand,
        credential: &GeaCredential,
        session: &GeaConversationSession,
    ) -> Result<GeaInteractionRequestReceipt, GeaError> {
        if command.action_id == "answer" {
            validate_question_answers(command.payload.as_ref())?;
        }
        let mut body = session.gateway_body();
        body["expectedVersion"] = Value::String(command.expected_version.trim().to_owned());
        body["idempotencyKey"] = Value::String(command.idempotency_key.trim().to_owned());
        body["actionId"] = Value::String(command.action_id.trim().to_owned());
        if let Some(payload) = command.payload {
            body["payload"] = payload;
        }
        let path = format!(
            "/ai/gateway/interaction-requests/{}/actions",
            encode_path_segment(request_id.trim())
        );
        let value = self
            .post_for_conversation(user_id, conversation_id, credential, &path, &body)
            .await?;
        let receipt = parse_receipt(&value, request_id)?;
        tracing::info!(
            user_id,
            conversation_id,
            request_id,
            version = receipt.version,
            status = ?receipt.status,
            audit_id = receipt.audit_id,
            "GEA interaction request action completed"
        );
        Ok(receipt)
    }

    async fn credential(&self, user_id: &str) -> Result<GeaCredential, GeaError> {
        self.credentials
            .read()
            .await
            .get(user_id)
            .cloned()
            .ok_or_else(GeaError::unauthenticated)
    }

    async fn session(&self, user_id: &str, conversation_id: &str) -> Result<GeaConversationSession, GeaError> {
        self.sessions
            .read()
            .await
            .get(&(user_id.to_owned(), conversation_id.to_owned()))
            .cloned()
            .ok_or_else(GeaError::session_required)
    }

    async fn clear_sessions(&self, user_id: &str) {
        self.sessions.write().await.retain(|(owner, _), _| owner != user_id);
        self.interaction_locks
            .lock()
            .await
            .retain(|(owner, _), _| owner != user_id);
    }

    async fn interaction_lock(&self, user_id: &str, conversation_id: &str) -> InteractionMutex {
        let key = (user_id.to_owned(), conversation_id.to_owned());
        let mut locks = self.interaction_locks.lock().await;
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn projection(&self) -> Result<&InteractionRequestProjection, GeaError> {
        self.projection
            .as_ref()
            .ok_or_else(|| GeaError::internal("Interaction Request 投影未配置"))
    }

    fn notification_projection(&self) -> Result<&NotificationProjection, GeaError> {
        self.notification_projection
            .as_ref()
            .ok_or_else(|| GeaError::internal("Notification 投影未配置"))
    }

    fn ensure_interaction_request_poll(&self, user_id: &str) {
        let Some(interval) = self.interaction_poll_interval else {
            return;
        };
        if self.projection.is_none() {
            return;
        }
        let service = self.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            let poll_id = Uuid::now_v7();
            let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
            let wake = Arc::new(tokio::sync::Notify::new());
            {
                let mut pollers = service.interaction_pollers.lock().await;
                if let Some(current) = pollers.get(&user_id) {
                    current.wake.notify_one();
                    return;
                }
                pollers.insert(
                    user_id.clone(),
                    InteractionPollControl {
                        id: poll_id,
                        stop,
                        wake: wake.clone(),
                    },
                );
            }
            tracing::info!(user_id, "GEA Interaction Request user sync started");
            let mut delay = Duration::ZERO;
            loop {
                if !delay.is_zero() {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = wake.notified() => {}
                        _ = stop_rx.changed() => break,
                    }
                }
                let owns_poll = service
                    .interaction_pollers
                    .lock()
                    .await
                    .get(&user_id)
                    .is_some_and(|current| current.id == poll_id);
                if !owns_poll || !service.credentials.read().await.contains_key(&user_id) {
                    break;
                }
                let synchronized = match service.list_all_interaction_requests(&user_id).await {
                    Ok(list) => list.sync_state == InteractionRequestSyncState::Complete,
                    Err(error) => {
                        tracing::warn!(
                            user_id,
                            code = %error.body.code,
                            "GEA interaction request background refresh failed"
                        );
                        false
                    }
                };
                if service.notification_projection.is_some() {
                    match service.list_notifications(&user_id, Some("active")).await {
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(
                                code = %error.body.code,
                                "GEA Notification background refresh failed"
                            );
                        }
                    }
                }
                delay = next_interaction_poll_delay(delay, interval, synchronized);
            }
            {
                let mut pollers = service.interaction_pollers.lock().await;
                if pollers.get(&user_id).is_some_and(|current| current.id == poll_id) {
                    pollers.remove(&user_id);
                }
            }
            tracing::info!(user_id, "GEA Interaction Request user sync stopped");
        });
    }

    async fn stop_interaction_request_poll(&self, user_id: &str) {
        if let Some(control) = self.interaction_pollers.lock().await.remove(user_id) {
            let _ = control.stop.send(true);
        }
    }

    async fn post_for_user(
        &self,
        user_id: &str,
        credential: &GeaCredential,
        path: &str,
        body: &Value,
    ) -> Result<Value, GeaError> {
        let result = self.post(credential, path, body).await.and_then(|value| {
            ensure_success(&value)?;
            Ok(value)
        });
        if matches!(&result, Err(error) if error.is_unauthorized()) {
            self.invalidate_auth_session(user_id).await;
        }
        result
    }

    async fn post_for_user_with_trace(
        &self,
        user_id: &str,
        credential: &GeaCredential,
        path: &str,
        body: &Value,
        trace_id: &str,
    ) -> Result<Value, GeaError> {
        let mut headers = self.user_headers(credential)?;
        headers.insert(
            "x-request-id",
            HeaderValue::from_str(trace_id).map_err(|_| GeaError::invalid_request("traceId 格式无效"))?,
        );
        let result = async {
            let response = self
                .client
                .post(format!("{}{}", self.base_url, path))
                .headers(headers)
                .json(body)
                .send()
                .await
                .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let value = response
                .json::<Value>()
                .await
                .map_err(|_| invalid_upstream("GEA 返回了无效 JSON"))?;
            if !status.is_success() {
                let mut error = upstream_business_error(&value, status.as_u16());
                error.body.retry_after_ms = retry_after_ms;
                return Err(error);
            }
            ensure_success(&value)?;
            Ok(value)
        }
        .await;
        if matches!(&result, Err(error) if error.is_unauthorized()) {
            self.invalidate_auth_session(user_id).await;
        }
        result
    }

    async fn post_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        credential: &GeaCredential,
        path: &str,
        body: &Value,
    ) -> Result<Value, GeaError> {
        let result = self.post_for_user(user_id, credential, path, body).await;
        if matches!(&result, Err(error) if error.body.category == "SESSION") {
            self.sessions
                .write()
                .await
                .remove(&(user_id.to_owned(), conversation_id.to_owned()));
        }
        result
    }

    async fn post(&self, credential: &GeaCredential, path: &str, body: &Value) -> Result<Value, GeaError> {
        let headers = self.user_headers(credential)?;
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
        let status = response.status();
        let retry_after_ms = parse_retry_after_ms(response.headers());
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| invalid_upstream("GEA 返回了无效 JSON"))?;
        if !status.is_success() {
            let mut error = upstream_business_error(&value, status.as_u16());
            error.body.retry_after_ms = retry_after_ms;
            return Err(error);
        }
        Ok(value)
    }

    async fn get_for_user_path(
        &self,
        user_id: &str,
        credential: &GeaCredential,
        path: &str,
        trace_id: &str,
    ) -> Result<Value, GeaError> {
        let mut headers = self.user_headers(credential)?;
        headers.insert(
            "x-request-id",
            HeaderValue::from_str(trace_id).map_err(|_| GeaError::invalid_request("traceId 格式无效"))?,
        );
        let result = async {
            let response = self
                .client
                .get(format!("{}{}", self.base_url, path))
                .headers(headers)
                .send()
                .await
                .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let value = response
                .json::<Value>()
                .await
                .map_err(|_| invalid_upstream("GEA 返回了无效 JSON"))?;
            if !status.is_success() {
                let mut error = upstream_business_error(&value, status.as_u16());
                error.body.retry_after_ms = retry_after_ms;
                return Err(error);
            }
            ensure_success(&value)?;
            Ok(value)
        }
        .await;
        if matches!(&result, Err(error) if error.is_unauthorized()) {
            self.invalidate_auth_session(user_id).await;
        }
        result
    }

    fn user_headers(&self, credential: &GeaCredential) -> Result<HeaderMap, GeaError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-access-token",
            HeaderValue::from_str(credential.access_token.as_ref())
                .map_err(|_| GeaError::invalid_request("GEA access token 格式无效"))?,
        );
        if let Some(tenant_id) = credential.tenant_id.as_deref() {
            headers.insert(
                "x-tenant-id",
                HeaderValue::from_str(tenant_id).map_err(|_| GeaError::invalid_request("GEA tenantId 格式无效"))?,
            );
        }
        Ok(headers)
    }

    async fn get_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        credential: &GeaCredential,
        session: &GeaConversationSession,
    ) -> Result<Value, GeaError> {
        let result = self.get(credential, session).await.and_then(|value| {
            ensure_success(&value)?;
            Ok(value)
        });
        if matches!(&result, Err(error) if error.is_unauthorized()) {
            self.invalidate_auth_session(user_id).await;
        }
        if matches!(&result, Err(error) if error.body.category == "SESSION") {
            self.sessions
                .write()
                .await
                .remove(&(user_id.to_owned(), conversation_id.to_owned()));
        }
        result
    }

    async fn get(&self, credential: &GeaCredential, session: &GeaConversationSession) -> Result<Value, GeaError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-access-token",
            HeaderValue::from_str(credential.access_token.as_ref())
                .map_err(|_| GeaError::invalid_request("GEA access token 格式无效"))?,
        );
        if let Some(tenant_id) = credential.tenant_id.as_deref() {
            headers.insert(
                "x-tenant-id",
                HeaderValue::from_str(tenant_id).map_err(|_| GeaError::invalid_request("GEA tenantId 格式无效"))?,
            );
        }
        headers.insert(
            "x-delegation-token",
            HeaderValue::from_str(session.delegation_token.as_ref())
                .map_err(|_| GeaError::invalid_request("GEA delegationToken 格式无效"))?,
        );
        let response = self
            .client
            .get(format!("{}/ai/gateway/interaction-requests", self.base_url))
            .headers(headers)
            .query(&[
                ("agentCode", session.agent_code.as_str()),
                ("sessionId", session.session_id.as_str()),
                ("conversationId", session.conversation_id.as_str()),
            ])
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
        let status = response.status();
        let retry_after_ms = parse_retry_after_ms(response.headers());
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| invalid_upstream("GEA 返回了无效 JSON"))?;
        if !status.is_success() {
            let mut error = upstream_business_error(&value, status.as_u16());
            error.body.retry_after_ms = retry_after_ms;
            return Err(error);
        }
        Ok(value)
    }
}

fn next_interaction_poll_delay(current: Duration, base: Duration, synchronized: bool) -> Duration {
    if synchronized {
        return base;
    }
    let maximum = INTERACTION_POLL_MAX_BACKOFF.max(base);
    current.max(base).saturating_mul(2).min(maximum)
}

impl GeaConversationSession {
    fn gateway_body(&self) -> Value {
        json!({
            "agentCode": self.agent_code,
            "sessionId": self.session_id,
            "conversationId": self.conversation_id,
            "delegationToken": self.delegation_token.as_ref()
        })
    }
}

fn encode_path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn non_empty(value: impl AsRef<str>) -> Option<String> {
    let value = value.as_ref().trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn required_string(value: &Value, field: &str) -> Result<String, GeaError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(non_empty)
        .ok_or_else(|| invalid_upstream(format!("GEA 响应缺少 {field}")))
}

fn ensure_success(value: &Value) -> Result<(), GeaError> {
    if value.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(upstream_business_error(value, 200))
    }
}

fn sanitize_tool_input_schema(mut schema: Value) -> Result<Value, GeaError> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(invalid_upstream("GEA Tool inputSchema 根节点必须为 object"));
    }
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.retain(|name, _| !is_gea_context_field(name));
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|name| name.as_str().is_none_or(|name| !is_gea_context_field(name)));
    }
    Ok(schema)
}

fn is_gea_context_field(name: &str) -> bool {
    let normalized = name
        .chars()
        .filter(|value| value.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    GEA_CONTEXT_FIELDS.contains(&normalized.as_str())
}

fn invalid_upstream(message: impl Into<String>) -> GeaError {
    GeaError::bad_gateway("GEA_INVALID_RESPONSE", message)
}

fn resume_delivery_unknown() -> GeaError {
    GeaError::conflict(
        "GEA_INTERACTION_RESUME_UNKNOWN",
        "原 Turn 恢复结果未知，已禁止自动重试，请回到原会话核验",
    )
}

fn access_denied_error(value: &Value) -> GeaError {
    let mut error = upstream_business_error(value, 403);
    if error.body.code == "GEA_UPSTREAM_ERROR" {
        error.body.code = value
            .pointer("/result/accessDecision/code")
            .and_then(value_as_string)
            .unwrap_or_else(|| "GEA_ACCESS_DENIED".to_owned());
    }
    error
}

fn upstream_business_error(value: &Value, fallback_status: u16) -> GeaError {
    let code = value
        .get("code")
        .and_then(value_as_string)
        .unwrap_or_else(|| "GEA_UPSTREAM_ERROR".to_owned());
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .and_then(non_empty)
        .unwrap_or_else(|| "GEA 请求未完成".to_owned());
    let category = value.get("category").and_then(Value::as_str).and_then(non_empty);
    let status = if (200..300).contains(&fallback_status) {
        status_for_category(category.as_deref())
    } else {
        fallback_status
    };
    let mut error = GeaError::from_http_status(status, code, message);
    if let Some(category) = category {
        error.body.category = category;
    }
    error.body.retryable = value
        .get("retryable")
        .and_then(Value::as_bool)
        .unwrap_or(error.body.retryable);
    error.body.retry_after_ms = value.get("retryAfterMs").and_then(Value::as_u64);
    error.body.request_id = value.get("requestId").and_then(Value::as_str).and_then(non_empty);
    error.body.trace_id = value.get("traceId").and_then(Value::as_str).and_then(non_empty);
    error.body.audit_id = value.get("auditId").and_then(Value::as_str).and_then(non_empty);
    error.body.details = value.get("details").cloned();
    error
}

fn status_for_category(category: Option<&str>) -> u16 {
    match category {
        Some("AUTHENTICATION") => 401,
        Some("AUTHORIZATION") => 403,
        Some("SESSION") | Some("CONFLICT") => 409,
        Some("VALIDATION") => 422,
        Some("RATE_LIMIT") => 429,
        Some("UPSTREAM") => 502,
        _ => 502,
    }
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => non_empty(value),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use aionui_api_types::{
        CreateGeaSessionRequest, GeaInteractionRequestActionCommand, GeaInteractionRequestKind,
        GeaInteractionRequestReceiptStatus, GeaInteractionRequestStatus, InteractionRequestActionCommand,
        InteractionRequestReceipt, InteractionRequestSyncState, SetGeaAuthSessionRequest,
    };
    use aionui_db::{SqliteConversationRepository, SqliteInteractionRequestRepository, init_database_memory};
    use aionui_realtime::BroadcastEventBus;
    use serde_json::json;
    use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::GeaService;

    async fn authenticated_service(server: &MockServer) -> GeaService {
        let service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service
            .set_auth_session(
                "user-1",
                SetGeaAuthSessionRequest {
                    access_token: "test-access-token".to_owned(),
                    tenant_id: Some("tenant-1".to_owned()),
                },
            )
            .await
            .unwrap();
        service
    }

    async fn projected_service(server: &MockServer) -> (GeaService, aionui_db::Database) {
        let database = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
             VALUES ('user-1', 'gea-test-user', 'not-used', 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations \
                (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
             VALUES ('conversation-1', 'user-1', 'GEA fixture', 'aionrs', '{}', 'running', 0, 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let service = authenticated_service(server)
            .await
            .with_interaction_request_projection(
                Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone())),
                Arc::new(SqliteConversationRepository::new(database.pool().clone())),
                Arc::new(BroadcastEventBus::new(32)),
                Some(Arc::new(|_| Some("turn-1".to_owned()))),
            )
            .with_interaction_turn_resumer(Arc::new(|_, _, _, _| Box::pin(async { Ok(()) })));
        (service, database)
    }

    async fn mount_question_snapshot(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "pending-r1",
                    "items": [{
                        "id": "request-question-1",
                        "version": "v1",
                        "status": "pending",
                        "kind": "question",
                        "title": "Choose a cost center",
                        "sourceLabel": "ERP",
                        "allowedActions": ["answer", "decline"],
                        "updatedAt": "2026-08-17T10:00:10+08:00",
                        "presentation": {
                            "type": "question",
                            "questions": [{
                                "header": "Cost center",
                                "question": "Which cost center?",
                                "multiSelect": false,
                                "options": [{ "label": "CC-100" }]
                            }]
                        }
                    }]
                }
            })))
            .mount(server)
            .await;
    }

    async fn mount_permission_snapshot(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "permission-r1",
                    "items": [{
                        "id": "request-permission-1",
                        "version": "v3",
                        "status": "pending",
                        "kind": "permission",
                        "title": "Confirm production submission",
                        "sourceLabel": "OA",
                        "allowedActions": ["proceed_once", "reject_once"],
                        "updatedAt": "2026-08-17T10:02:00+08:00",
                        "presentation": {
                            "type": "permission",
                            "title": "Confirm production submission",
                            "description": "Submit the reviewed request once.",
                            "operation": "execute",
                            "detail": "test-environment",
                            "options": [
                                { "label": "Allow once", "value": "proceed_once" },
                                { "label": "Reject", "value": "reject_once" }
                            ]
                        }
                    }]
                }
            })))
            .mount(server)
            .await;
    }

    async fn mount_session(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .and(header("x-access-token", "test-access-token"))
            .and(header("x-tenant-id", "tenant-1"))
            .and(body_partial_json(json!({
                "consumerType": "AGENT",
                "consumerCode": "agent-sales",
                "conversationId": "conversation-1",
                "channel": "AION_CORE",
                "preparationId": "preparation-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret",
                    "effectiveCapabilityCodes": ["MCP_TOOL:cube:query_business_data"],
                    "gatewayContext": {
                        "consumerCode": "agent-sales",
                        "sessionId": "gea-session-1",
                        "conversationId": "conversation-1"
                    }
                }
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_recoverable_session(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .and(header("x-access-token", "test-access-token"))
            .and(body_partial_json(json!({
                "consumerCode": "agent-sales",
                "conversationId": "conversation-1",
                "preparationId": "preparation-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret",
                    "gatewayContext": {
                        "consumerCode": "agent-sales",
                        "sessionId": "gea-session-recovered",
                        "conversationId": "conversation-1"
                    }
                }
            })))
            .expect(2)
            .mount(server)
            .await;
    }

    async fn create_session(service: &GeaService) {
        service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: Some("preparation-1".to_owned()),
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rehydrated_auth_recovers_a_session_before_any_request_was_projected() {
        let server = MockServer::start().await;
        mount_recoverable_session(&server).await;
        mount_question_snapshot(&server).await;
        let (service, database) = projected_service(&server).await;
        create_session(&service).await;
        drop(service);

        let restarted = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_interaction_request_projection(
                Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone())),
                Arc::new(SqliteConversationRepository::new(database.pool().clone())),
                Arc::new(BroadcastEventBus::new(32)),
                Some(Arc::new(|_| Some("turn-1".to_owned()))),
            )
            .with_interaction_turn_resumer(Arc::new(|_, _, _, _| Box::pin(async { Ok(()) })));
        restarted
            .set_auth_session(
                "user-1",
                SetGeaAuthSessionRequest {
                    access_token: "test-access-token".to_owned(),
                    tenant_id: Some("tenant-1".to_owned()),
                },
            )
            .await
            .unwrap();

        let pending = restarted.list_all_interaction_requests("user-1").await.unwrap();
        assert_eq!(pending.items.len(), 1);
        assert_eq!(pending.items[0].id, "request-question-1");
    }

    #[tokio::test]
    async fn global_action_replay_returns_one_persisted_receipt_and_one_upstream_write() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .and(body_partial_json(json!({
                "expectedVersion": "v1",
                "idempotencyKey": "interaction:request-question-1:v1:answer",
                "actionId": "answer"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-1",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "accepted",
                    "resolvedAt": "2026-08-17T10:01:00+08:00"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, _database) = projected_service(&server).await;
        let resumed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = service.with_interaction_turn_resumer({
            let resumed = Arc::clone(&resumed);
            Arc::new(move |user_id, conversation_id, turn_id, receipt| {
                let resumed = Arc::clone(&resumed);
                Box::pin(async move {
                    resumed
                        .lock()
                        .unwrap()
                        .push((user_id, conversation_id, turn_id, receipt.receipt_id));
                    Ok(())
                })
            })
        });
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        let command = InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: "interaction:request-question-1:v1:answer".to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let first = service
            .act_on_global_interaction_request("user-1", "request-question-1", command.clone())
            .await
            .unwrap();
        let replay = service
            .act_on_global_interaction_request("user-1", "request-question-1", command)
            .await
            .unwrap();
        assert_eq!(first, replay);
        assert_eq!(first.status, GeaInteractionRequestReceiptStatus::Accepted);
        assert_eq!(
            resumed.lock().unwrap().as_slice(),
            &[(
                "user-1".to_owned(),
                "conversation-1".to_owned(),
                "turn-1".to_owned(),
                "receipt-1".to_owned(),
            )]
        );
        assert!(
            service
                .projection()
                .unwrap()
                .list_active("user-1")
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[tokio::test]
    async fn concurrent_equivalent_actions_with_different_keys_share_one_upstream_write() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(30))
                    .set_body_json(json!({
                        "success": true,
                        "result": {
                            "receiptId": "receipt-concurrent",
                            "requestId": "request-question-1",
                            "version": "v2",
                            "status": "accepted"
                        }
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (service, _database) = projected_service(&server).await;
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        let command = |key: &str| InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: key.to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let (first, second) = tokio::join!(
            service.act_on_global_interaction_request("user-1", "request-question-1", command("command-a")),
            service.act_on_global_interaction_request("user-1", "request-question-1", command("command-b"))
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.receipt_id, "receipt-concurrent");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn failed_turn_delivery_becomes_unknown_and_is_not_automatically_replayed() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-resume-retry",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "accepted"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, _database) = projected_service(&server).await;
        let resume_attempts = Arc::new(AtomicUsize::new(0));
        let service = service.with_interaction_turn_resumer({
            let resume_attempts = Arc::clone(&resume_attempts);
            Arc::new(move |_, _, _, _| {
                resume_attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Err("turn delivery result unavailable".to_owned()) })
            })
        });
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        let command = InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: "resume-retry".to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let first = service
            .act_on_global_interaction_request("user-1", "request-question-1", command.clone())
            .await
            .unwrap_err();
        assert_eq!(first.body.code, "GEA_INTERACTION_RESUME_UNKNOWN");
        assert_eq!(
            service
                .projection()
                .unwrap()
                .list_active("user-1")
                .await
                .unwrap()
                .items
                .len(),
            1
        );

        let replay = service
            .act_on_global_interaction_request("user-1", "request-question-1", command)
            .await
            .unwrap_err();
        assert_eq!(replay.body.code, "GEA_INTERACTION_RESUME_UNKNOWN");
        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn slow_turn_resume_becomes_unknown_before_the_claim_lease_and_is_not_replayed() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-resume-timeout",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "accepted"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, database) = projected_service(&server).await;
        let resume_attempts = Arc::new(AtomicUsize::new(0));
        let service = service.with_interaction_turn_resumer({
            let resume_attempts = Arc::clone(&resume_attempts);
            Arc::new(move |_, _, _, _| {
                resume_attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Ok(())
                })
            })
        });
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();

        let error = service
            .act_on_global_interaction_request(
                "user-1",
                "request-question-1",
                InteractionRequestActionCommand {
                    expected_version: "v1".to_owned(),
                    idempotency_key: "resume-timeout".to_owned(),
                    action_id: "answer".to_owned(),
                    payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.body.code, "GEA_INTERACTION_RESUME_UNKNOWN");
        let state: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT resume_claim_owner IS NOT NULL, resume_started_at IS NOT NULL, \
                    resume_delivered_at IS NOT NULL, finalized_at IS NOT NULL \
             FROM gea_interaction_request_receipts WHERE idempotency_key = 'resume-timeout'",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(state, (1, 1, 0, 0));
        let replay = service
            .act_on_global_interaction_request(
                "user-1",
                "request-question-1",
                InteractionRequestActionCommand {
                    expected_version: "v1".to_owned(),
                    idempotency_key: "resume-timeout".to_owned(),
                    action_id: "answer".to_owned(),
                    payload: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(replay.body.code, "GEA_INTERACTION_RESUME_UNKNOWN");
        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delivered_marker_failure_never_replays_the_turn_callback() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-marker-failure",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "accepted"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, database) = projected_service(&server).await;
        let resume_attempts = Arc::new(AtomicUsize::new(0));
        let service = service.with_interaction_turn_resumer({
            let resume_attempts = Arc::clone(&resume_attempts);
            Arc::new(move |_, _, _, _| {
                resume_attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
        });
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_interaction_resume_delivered \
             BEFORE UPDATE OF resume_delivered_at ON gea_interaction_request_receipts \
             WHEN NEW.resume_delivered_at IS NOT NULL \
             BEGIN SELECT RAISE(FAIL, 'simulated delivered marker failure'); END",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let command = InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: "marker-failure".to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let first = service
            .act_on_global_interaction_request("user-1", "request-question-1", command.clone())
            .await
            .unwrap_err();
        assert_eq!(first.body.code, "GEA_INTERACTION_REQUEST_STORAGE_ERROR");
        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
        sqlx::query("DROP TRIGGER fail_interaction_resume_delivered")
            .execute(database.pool())
            .await
            .unwrap();

        let replay = service
            .act_on_global_interaction_request("user-1", "request-question-1", command)
            .await
            .unwrap_err();
        assert_eq!(replay.body.code, "GEA_INTERACTION_RESUME_UNKNOWN");
        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn finalize_storage_failure_replays_without_delivering_the_turn_twice() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-finalize-retry",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "accepted"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, database) = projected_service(&server).await;
        let resume_attempts = Arc::new(AtomicUsize::new(0));
        let service = service.with_interaction_turn_resumer({
            let resume_attempts = Arc::clone(&resume_attempts);
            Arc::new(move |_, _, _, _| {
                resume_attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
        });
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_interaction_receipt_finalize \
             BEFORE UPDATE OF finalized_at ON gea_interaction_request_receipts \
             WHEN NEW.finalized_at IS NOT NULL \
             BEGIN SELECT RAISE(FAIL, 'simulated finalize failure'); END",
        )
        .execute(database.pool())
        .await
        .unwrap();
        let command = InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: "finalize-retry".to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let first = service
            .act_on_global_interaction_request("user-1", "request-question-1", command.clone())
            .await
            .unwrap_err();
        assert_eq!(first.body.code, "GEA_INTERACTION_REQUEST_STORAGE_ERROR");
        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
        sqlx::query("DROP TRIGGER fail_interaction_receipt_finalize")
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query(
            "UPDATE gea_interaction_request_receipts SET resume_claimed_at = 1 \
             WHERE idempotency_key = 'finalize-retry'",
        )
        .execute(database.pool())
        .await
        .unwrap();

        let replay = service
            .act_on_global_interaction_request("user-1", "request-question-1", command)
            .await
            .unwrap();
        assert_eq!(replay.receipt_id, "receipt-finalize-retry");
        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_new_service_owner_automatically_recovers_a_claim_left_before_turn_delivery() {
        let server = MockServer::start().await;
        mount_recoverable_session(&server).await;
        mount_question_snapshot(&server).await;
        let (service, database) = projected_service(&server).await;
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        let request = service
            .projection()
            .unwrap()
            .find("user-1", "request-question-1")
            .await
            .unwrap();
        let receipt = InteractionRequestReceipt {
            receipt_id: "receipt-crashed-owner".to_owned(),
            request_id: "request-question-1".to_owned(),
            version: "v2".to_owned(),
            status: GeaInteractionRequestReceiptStatus::Accepted,
            turn_continuation: Some(aionui_api_types::GeaInteractionTurnContinuation::OriginalToolCallReleased),
            resolved_at: None,
            resolved_by: None,
            request: Some(request),
        };
        sqlx::query(
            "INSERT INTO gea_interaction_request_receipts \
                (user_id, request_id, idempotency_key, expected_version, action_id, receipt, created_at, \
                 resume_claim_owner, resume_claimed_at) \
             VALUES ('user-1', 'request-question-1', 'crashed-owner', 'v1', 'answer', ?, 1, 'dead-process', ?)",
        )
        .bind(serde_json::to_string(&receipt).unwrap())
        .bind(1_i64)
        .execute(database.pool())
        .await
        .unwrap();

        let resume_attempts = Arc::new(AtomicUsize::new(0));
        let restarted = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_interaction_request_projection(
                Arc::new(SqliteInteractionRequestRepository::new(database.pool().clone())),
                Arc::new(SqliteConversationRepository::new(database.pool().clone())),
                Arc::new(BroadcastEventBus::new(32)),
                Some(Arc::new(|_| Some("turn-1".to_owned()))),
            )
            .with_interaction_turn_resumer({
                let resume_attempts = Arc::clone(&resume_attempts);
                Arc::new(move |_, _, _, _| {
                    resume_attempts.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                })
            });
        restarted
            .set_auth_session(
                "user-1",
                SetGeaAuthSessionRequest {
                    access_token: "test-access-token".to_owned(),
                    tenant_id: Some("tenant-1".to_owned()),
                },
            )
            .await
            .unwrap();

        assert_eq!(resume_attempts.load(Ordering::SeqCst), 1);
        let finalized_at: Option<i64> = sqlx::query_scalar(
            "SELECT finalized_at FROM gea_interaction_request_receipts \
             WHERE idempotency_key = 'crashed-owner'",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert!(finalized_at.is_some());
        assert!(
            restarted
                .projection()
                .unwrap()
                .list_active("user-1")
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[tokio::test]
    async fn stale_global_action_reaches_gea_and_uses_its_conflict_receipt() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .and(body_partial_json(json!({
                "expectedVersion": "stale-v0",
                "idempotencyKey": "stale-command",
                "actionId": "answer"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "stale-conflict-receipt",
                    "requestId": "request-question-1",
                    "version": "v1",
                    "status": "conflict"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, _database) = projected_service(&server).await;
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();

        let receipt = service
            .act_on_global_interaction_request(
                "user-1",
                "request-question-1",
                InteractionRequestActionCommand {
                    expected_version: "stale-v0".to_owned(),
                    idempotency_key: "stale-command".to_owned(),
                    action_id: "answer".to_owned(),
                    payload: Some(json!({
                        "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }]
                    })),
                },
            )
            .await
            .unwrap();
        assert_eq!(receipt.status, GeaInteractionRequestReceiptStatus::Conflict);
        assert_eq!(receipt.request.unwrap().version, "v1");
    }

    #[tokio::test]
    async fn global_action_uses_another_valid_session_without_moving_the_navigation_anchor() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .and(body_partial_json(json!({
                "consumerCode": "agent-sales",
                "conversationId": "conversation-2"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret-2",
                    "gatewayContext": {
                        "consumerCode": "agent-sales",
                        "sessionId": "gea-session-2",
                        "conversationId": "conversation-2"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .and(body_partial_json(json!({
                "sessionId": "gea-session-2",
                "conversationId": "conversation-2",
                "expectedVersion": "v1",
                "idempotencyKey": "cross-session-action",
                "actionId": "answer"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "cross-session-processing",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "processing"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (service, database) = projected_service(&server).await;
        sqlx::query(
            "INSERT INTO conversations \
                (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
             VALUES ('conversation-2', 'user-1', 'GEA fixture 2', 'aionrs', '{}', 'running', 0, 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        service
            .create_session(
                "user-1",
                "conversation-2",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap();
        service
            .sessions
            .write()
            .await
            .remove(&("user-1".to_owned(), "conversation-1".to_owned()));

        let receipt = service
            .act_on_global_interaction_request(
                "user-1",
                "request-question-1",
                InteractionRequestActionCommand {
                    expected_version: "v1".to_owned(),
                    idempotency_key: "cross-session-action".to_owned(),
                    action_id: "answer".to_owned(),
                    payload: Some(json!({
                        "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }]
                    })),
                },
            )
            .await
            .unwrap();

        assert_eq!(receipt.status, GeaInteractionRequestReceiptStatus::Processing);
        let request = receipt.request.unwrap();
        assert_eq!(request.conversation_id, "conversation-1");
        assert_eq!(request.status, GeaInteractionRequestStatus::Processing);
    }

    #[tokio::test]
    async fn authoritative_refresh_failure_preserves_and_replays_the_upstream_conflict_receipt() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "pending-r1",
                    "items": [{
                        "id": "request-question-1",
                        "version": "v1",
                        "status": "pending",
                        "kind": "question",
                        "title": "Choose a cost center",
                        "allowedActions": ["answer", "decline"],
                        "updatedAt": "2026-08-17T10:00:10+08:00",
                        "presentation": {
                            "type": "question",
                            "questions": [{
                                "question": "Which cost center?",
                                "multiSelect": false,
                                "options": [{ "label": "CC-100" }]
                            }]
                        }
                    }]
                }
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .respond_with(ResponseTemplate::new(502).set_body_json(json!({
                "code": "GEA_REFRESH_UNAVAILABLE",
                "message": "refresh unavailable",
                "category": "UPSTREAM"
            })))
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-conflict-refresh-failed",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "conflict"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, _database) = projected_service(&server).await;
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        let command = InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: "conflict-refresh-failed".to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let first = service
            .act_on_global_interaction_request("user-1", "request-question-1", command.clone())
            .await
            .unwrap();
        assert_eq!(first.status, GeaInteractionRequestReceiptStatus::Conflict);
        assert_eq!(first.request.as_ref().unwrap().conversation_id, "conversation-1");

        let replay = service
            .act_on_global_interaction_request("user-1", "request-question-1", command)
            .await
            .unwrap();
        assert_eq!(replay, first);
    }

    #[tokio::test]
    async fn active_session_poll_syncs_immediately_without_client_polling() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        let (mut service, _database) = projected_service(&server).await;
        service.interaction_poll_interval = Some(Duration::from_secs(60 * 60));
        create_session(&service).await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !service
                    .projection()
                    .unwrap()
                    .list_active("user-1")
                    .await
                    .unwrap()
                    .items
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("background poll should project the pending request immediately");
        service.clear_auth_session("user-1").await;
    }

    #[tokio::test]
    async fn interaction_request_poll_is_singleton_per_user_and_stops_on_logout() {
        let server = MockServer::start().await;
        let (mut service, _database) = projected_service(&server).await;
        service.interaction_poll_interval = Some(Duration::from_secs(60 * 60));

        service.ensure_interaction_request_poll("user-1");
        service.ensure_interaction_request_poll("user-1");
        tokio::task::yield_now().await;

        assert_eq!(service.interaction_pollers.lock().await.len(), 1);
        service.clear_auth_session("user-1").await;
        assert!(service.interaction_pollers.lock().await.is_empty());
    }

    #[tokio::test]
    async fn session_creation_wakes_an_existing_user_poll_immediately() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_question_snapshot(&server).await;
        let (mut service, _database) = projected_service(&server).await;
        service.interaction_poll_interval = Some(Duration::from_secs(60 * 60));
        service.ensure_interaction_request_poll("user-1");
        tokio::time::sleep(Duration::from_millis(50)).await;

        create_session(&service).await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !service
                    .projection()
                    .unwrap()
                    .list_active("user-1")
                    .await
                    .unwrap()
                    .items
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("session creation should wake the existing user poll");
        service.clear_auth_session("user-1").await;
    }

    #[test]
    fn interaction_request_poll_delay_resets_and_caps_after_failures() {
        let base = Duration::from_secs(3);

        assert_eq!(super::next_interaction_poll_delay(Duration::ZERO, base, true), base);
        assert_eq!(
            super::next_interaction_poll_delay(Duration::ZERO, base, false),
            Duration::from_secs(6)
        );
        assert_eq!(
            super::next_interaction_poll_delay(Duration::from_secs(24), base, false),
            Duration::from_secs(30)
        );
        assert_eq!(
            super::next_interaction_poll_delay(Duration::from_secs(30), base, true),
            base
        );
    }

    #[tokio::test]
    async fn user_list_uses_one_complete_snapshot_across_sessions() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .and(body_partial_json(json!({
                "consumerCode": "agent-finance",
                "conversationId": "conversation-2"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret-2",
                    "gatewayContext": {
                        "consumerCode": "agent-finance",
                        "sessionId": "gea-session-2",
                        "conversationId": "conversation-2"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .and(query_param("conversationId", "conversation-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "principal-agent-r1",
                    "items": [
                        {
                            "requestId": "request-pending",
                            "version": "v1",
                            "status": "pending",
                            "kind": "question",
                            "title": "Question from the complete snapshot",
                            "allowedActions": ["answer", "decline"],
                            "updatedAt": "2026-08-17T10:00:00+08:00",
                            "presentation": "{\"type\":\"question\",\"questions\":[{\"question\":\"Continue?\",\"options\":[{\"label\":\"Yes\"}]}]}"
                        },
                        {
                            "requestId": "request-processing",
                            "version": "v2",
                            "status": "processing",
                            "kind": "permission",
                            "title": "Processing from the complete snapshot",
                            "allowedActions": ["proceed_once"],
                            "updatedAt": "2026-08-17T09:00:00+08:00",
                            "presentation": "{\"type\":\"permission\"}"
                        }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (service, database) = projected_service(&server).await;
        sqlx::query(
            "INSERT INTO conversations \
                (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
             VALUES ('conversation-2', 'user-1', 'GEA fixture 2', 'aionrs', '{}', 'running', 0, 1, 1)",
        )
        .execute(database.pool())
        .await
        .unwrap();
        create_session(&service).await;
        service
            .create_session(
                "user-1",
                "conversation-2",
                CreateGeaSessionRequest {
                    consumer_code: "agent-finance".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap();
        let list = service.list_all_interaction_requests("user-1").await.unwrap();

        assert_eq!(list.sync_state, InteractionRequestSyncState::Complete);
        assert_eq!(list.failed_session_count, 0);
        assert!(list.failure_codes.is_empty());
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].id, "request-pending");
        assert!(!list.items[0].stale);
        let processing_status: String =
            sqlx::query_scalar("SELECT status FROM gea_interaction_requests WHERE request_id = 'request-processing'")
                .fetch_one(database.pool())
                .await
                .unwrap();
        assert_eq!(processing_status, "processing");
    }

    #[tokio::test]
    async fn permission_accepts_only_the_current_allowed_action() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_permission_snapshot(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-permission-1/actions"))
            .and(body_partial_json(json!({
                "expectedVersion": "v3",
                "idempotencyKey": "permission-forbidden",
                "actionId": "proceed_always"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "permission-forbidden-receipt",
                    "requestId": "request-permission-1",
                    "version": "v3",
                    "status": "forbidden"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-permission-1/actions"))
            .and(body_partial_json(json!({
                "expectedVersion": "v3",
                "idempotencyKey": "permission-allowed",
                "actionId": "proceed_once"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "permission-receipt-1",
                    "requestId": "request-permission-1",
                    "version": "v4",
                    "status": "already_resolved",
                    "resolvedAt": "2026-08-17T10:03:00+08:00"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, _database) = projected_service(&server).await;
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();

        let forbidden = service
            .act_on_global_interaction_request(
                "user-1",
                "request-permission-1",
                InteractionRequestActionCommand {
                    expected_version: "v3".to_owned(),
                    idempotency_key: "permission-forbidden".to_owned(),
                    action_id: "proceed_always".to_owned(),
                    payload: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status, GeaInteractionRequestReceiptStatus::Forbidden);

        let accepted = service
            .act_on_global_interaction_request(
                "user-1",
                "request-permission-1",
                InteractionRequestActionCommand {
                    expected_version: "v3".to_owned(),
                    idempotency_key: "permission-allowed".to_owned(),
                    action_id: "proceed_once".to_owned(),
                    payload: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(accepted.status, GeaInteractionRequestReceiptStatus::AlreadyResolved);
    }

    #[tokio::test]
    async fn unknown_external_write_stays_active_for_authoritative_verification() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "pending-r1",
                    "items": [{
                        "requestId": "request-question-1",
                        "version": "v1",
                        "status": "pending",
                        "kind": "question",
                        "title": "Choose a cost center",
                        "allowedActions": ["answer", "decline"],
                        "presentation": {
                            "type": "question",
                            "questions": [{
                                "question": "Which cost center?",
                                "options": [{ "label": "CC-100" }]
                            }]
                        }
                    }]
                }
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "verification-r2",
                    "items": [{
                        "requestId": "request-question-1",
                        "version": "v2",
                        "status": "verification_required",
                        "kind": "permission",
                        "title": "Verify the external write",
                        "allowedActions": ["verify_succeeded", "verify_failed"],
                        "presentation": {
                            "type": "permission",
                            "title": "Verify the external write",
                            "description": "Confirm the outcome before continuing.",
                            "operation": "verify",
                            "options": [
                                { "label": "Succeeded", "value": "verify_succeeded" },
                                { "label": "Failed", "value": "verify_failed" }
                            ]
                        }
                    }]
                }
            })))
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .and(body_partial_json(json!({
                "expectedVersion": "v1",
                "idempotencyKey": "unknown-command",
                "actionId": "answer"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "unknown-receipt-1",
                    "requestId": "request-question-1",
                    "version": "v2",
                    "status": "unknown_external_write"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/interaction-requests/request-question-1/actions"))
            .and(body_partial_json(json!({
                "expectedVersion": "v2",
                "idempotencyKey": "verify-command",
                "actionId": "verify_succeeded"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "verification-receipt-2",
                    "requestId": "request-question-1",
                    "version": "v3",
                    "status": "accepted"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (service, database) = projected_service(&server).await;
        create_session(&service).await;
        service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();
        let command = InteractionRequestActionCommand {
            expected_version: "v1".to_owned(),
            idempotency_key: "unknown-command".to_owned(),
            action_id: "answer".to_owned(),
            payload: Some(json!({ "answers": [{ "question": "Which cost center?", "labels": ["CC-100"] }] })),
        };

        let first = service
            .act_on_global_interaction_request("user-1", "request-question-1", command.clone())
            .await
            .unwrap();
        let replay = service
            .act_on_global_interaction_request("user-1", "request-question-1", command)
            .await
            .unwrap();
        assert_eq!(first, replay);
        assert_eq!(first.status, GeaInteractionRequestReceiptStatus::UnknownExternalWrite);
        assert_eq!(
            first.request.unwrap().status,
            GeaInteractionRequestStatus::VerificationRequired
        );
        let active = service.projection().unwrap().list_active("user-1").await.unwrap();
        assert_eq!(active.items.len(), 1);
        assert_eq!(
            active.items[0].status,
            GeaInteractionRequestStatus::VerificationRequired
        );
        assert_eq!(active.items[0].allowed_actions, ["verify_succeeded", "verify_failed"]);
        let active_message_type: String = sqlx::query_scalar(
            "SELECT type FROM messages WHERE id = ( \
                SELECT message_id FROM gea_interaction_requests WHERE request_id = 'request-question-1' \
             )",
        )
        .fetch_one(database.pool())
        .await
        .unwrap();
        assert_eq!(active_message_type, "permission");

        let verified = service
            .act_on_global_interaction_request(
                "user-1",
                "request-question-1",
                InteractionRequestActionCommand {
                    expected_version: "v2".to_owned(),
                    idempotency_key: "verify-command".to_owned(),
                    action_id: "verify_succeeded".to_owned(),
                    payload: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(verified.status, GeaInteractionRequestReceiptStatus::Accepted);
        assert!(
            service
                .projection()
                .unwrap()
                .list_active("user-1")
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    async fn mount_query_tool(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "tools": [{
                    "name": "query_business_data",
                    "sourceCode": "cube",
                    "description": "Query business data",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "queries": { "type": "array" }
                        },
                        "required": ["queries"]
                    }
                }]
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn unified_session_uses_agent_consumer_and_preparation() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        let service = authenticated_service(&server).await;

        let response = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: Some("preparation-1".to_owned()),
                },
            )
            .await
            .unwrap();

        assert_eq!(response.session_id, "gea-session-1");
        assert_eq!(response.conversation_id, "conversation-1");
        assert_eq!(response.consumer_code, "agent-sales");
    }

    #[tokio::test]
    async fn session_falls_back_to_the_deployed_agent_endpoint_when_unified_path_is_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "code": "404",
                "message": "路径不存在，请检查路径是否正确",
                "category": "UPSTREAM",
                "retryable": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/agent/session"))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "channel": "CS_CLIENT"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret",
                    "effectiveCapabilityCodes": ["MCP_TOOL:cube:query_business_data"],
                    "gatewayContext": {
                        "agentId": "agent-sales",
                        "sessionId": "gea-session-legacy",
                        "conversationId": "gea-conversation-legacy"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let response = service
            .create_session(
                "user-1",
                "conversation-local",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .expect("fallback session");

        assert_eq!(response.session_id, "gea-session-legacy");
        assert_eq!(response.conversation_id, "gea-conversation-legacy");
        assert_eq!(response.consumer_code, "agent-sales");
    }

    #[tokio::test]
    async fn tool_call_keeps_gateway_context_out_of_business_arguments() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/list"))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "sessionId": "gea-session-1",
                "conversationId": "conversation-1",
                "delegationToken": "delegation-secret"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "tools": [{
                    "name": "query_business_data",
                    "sourceCode": "cube",
                    "description": "Query business data",
                    "inputSchema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "queries": { "type": "array" },
                            "sessionId": { "type": "string" },
                            "delegation_token": { "type": "string" }
                        },
                        "required": ["queries", "sessionId", "delegation_token"]
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "sessionId": "gea-session-1",
                "conversationId": "conversation-1",
                "delegationToken": "delegation-secret",
                "mcpCode": "cube",
                "toolName": "query_business_data",
                "arguments": {
                    "queries": [{ "measures": ["sales"] }]
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "sourceCode": "cube",
                "toolName": "query_business_data",
                "auditId": "audit-1",
                "result": { "rows": [] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let service = authenticated_service(&server).await;
        create_session(&service).await;
        let tools = service.list_tools("user-1", "conversation-1").await.unwrap();
        assert_eq!(tools[0].input_schema["required"], json!(["queries"]));
        assert_eq!(
            tools[0].input_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["queries"]
        );
        let response = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [{ "measures": ["sales"] }] }),
            )
            .await
            .unwrap();

        assert_eq!(response.audit_id.as_deref(), Some("audit-1"));
        assert_eq!(response.result, json!({ "rows": [] }));
    }

    #[tokio::test]
    async fn mcp_connection_test_creates_a_temporary_session_and_lists_real_tools() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .and(body_partial_json(json!({
                "consumerCode": "agent-sales",
                "conversationId": "gea-probe-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret",
                    "gatewayContext": {
                        "consumerCode": "agent-sales",
                        "sessionId": "gea-session-probe",
                        "conversationId": "gea-probe-1"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/list"))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "sessionId": "gea-session-probe",
                "conversationId": "gea-probe-1",
                "delegationToken": "delegation-secret"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "tools": [{
                    "name": "query_business_data",
                    "sourceCode": "cube",
                    "description": "Query business data",
                    "inputSchema": { "type": "object" }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let tools = service
            .test_mcp_connection_with_id("user-1", "agent-sales".to_owned(), "gea-probe-1".to_owned())
            .await
            .unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "query_business_data");
        assert!(
            service
                .sessions
                .read()
                .await
                .get(&("user-1".to_owned(), "gea-probe-1".to_owned()))
                .is_none(),
            "connection tests must not retain a conversation session"
        );
    }

    #[test]
    fn context_only_tool_schema_becomes_an_empty_business_object() {
        let schema = super::sanitize_tool_input_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agentCode": { "type": "string" },
                "conversation_id": { "type": "string" },
                "mcpCode": { "type": "string" }
            },
            "required": ["agentCode", "conversation_id", "mcpCode"]
        }))
        .unwrap();

        assert_eq!(schema["properties"], json!({}));
        assert_eq!(schema["required"], json!([]));
    }

    #[tokio::test]
    async fn successful_http_business_error_maps_from_gea_category() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "code": "AI_GATEWAY_DATA_PERMISSION_DENIED",
                "message": "Capability data governance is incomplete",
                "category": "AUTHORIZATION",
                "retryable": false,
                "requestId": "request-1",
                "traceId": "trace-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let error = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(error.body.code, "AI_GATEWAY_DATA_PERMISSION_DENIED");
        assert_eq!(error.body.request_id.as_deref(), Some("request-1"));
        assert!(service.auth_status("user-1").await.authenticated);
    }

    #[tokio::test]
    async fn denied_access_decision_returns_forbidden_without_session_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": {
                        "allowed": false,
                        "code": "AI_GATEWAY_AGENT_NOT_ALLOWED"
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let error = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(error.body.code, "AI_GATEWAY_AGENT_NOT_ALLOWED");
        assert_eq!(error.body.category, "AUTHORIZATION");
    }

    #[tokio::test]
    async fn upstream_authentication_failure_clears_cached_credentials_and_sessions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false,
                "code": "401",
                "message": "Token expired",
                "category": "AUTHENTICATION",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let error = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
        let status = service.auth_status("user-1").await;
        assert!(!status.authenticated);
        assert!(status.reauth_required);
        assert_eq!(
            service
                .create_session(
                    "user-1",
                    "conversation-1",
                    CreateGeaSessionRequest {
                        consumer_code: "agent-sales".to_owned(),
                        preparation_id: None,
                    },
                )
                .await
                .unwrap_err()
                .body
                .code,
            "GEA_AUTH_REQUIRED"
        );

        service
            .set_auth_session(
                "user-1",
                SetGeaAuthSessionRequest {
                    access_token: "replacement-access-token".to_owned(),
                    tenant_id: Some("tenant-1".to_owned()),
                },
            )
            .await
            .unwrap();
        let replacement_status = service.auth_status("user-1").await;
        assert!(replacement_status.authenticated);
        assert!(!replacement_status.reauth_required);
    }

    #[tokio::test]
    async fn tool_authentication_failure_clears_cached_credentials_and_sessions() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false,
                "code": "GEA_TOKEN_EXPIRED",
                "message": "Token expired",
                "category": "AUTHENTICATION",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
        let status = service.auth_status("user-1").await;
        assert!(!status.authenticated);
        assert!(status.reauth_required);
        let session_error = match service.session("user-1", "conversation-1").await {
            Ok(_) => panic!("authentication failure must discard the cached session"),
            Err(error) => error,
        };
        assert_eq!(session_error.body.code, "GEA_SESSION_REQUIRED");
    }

    #[tokio::test]
    async fn session_failure_discards_only_the_expired_conversation_session() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "success": false,
                "code": "GEA_SESSION_STALE",
                "message": "Session authorization revision is stale",
                "category": "SESSION",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::CONFLICT);
        assert!(service.auth_status("user-1").await.authenticated);
        let session_error = match service.session("user-1", "conversation-1").await {
            Ok(_) => panic!("session failure must discard the expired conversation session"),
            Err(error) => error,
        };
        assert_eq!(session_error.body.code, "GEA_SESSION_REQUIRED");
    }

    #[tokio::test]
    async fn rate_limit_preserves_retry_after_and_the_current_session() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "2")
                    .set_body_json(json!({
                        "success": false,
                        "code": "AI_GATEWAY_RATE_LIMITED",
                        "message": "Rate limited",
                        "category": "RATE_LIMIT",
                        "retryable": true,
                        "retryAfterMs": 500
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.body.code, "AI_GATEWAY_RATE_LIMITED");
        assert_eq!(error.body.category, "RATE_LIMIT");
        assert!(error.body.retryable);
        assert_eq!(error.body.retry_after_ms, Some(2_000));
        assert!(service.auth_status("user-1").await.authenticated);
        assert!(service.session("user-1", "conversation-1").await.is_ok());
    }

    #[tokio::test]
    async fn business_conflict_preserves_the_current_session() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "success": false,
                "code": "GEA_IDEMPOTENCY_CONFLICT",
                "message": "Conflicting request",
                "category": "CONFLICT",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(error.body.category, "CONFLICT");
        assert!(service.session("user-1", "conversation-1").await.is_ok());
    }

    #[tokio::test]
    async fn interaction_snapshot_uses_the_existing_gateway_session_context() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("GET"))
            .and(path("/ai/gateway/interaction-requests"))
            .and(query_param("agentCode", "agent-sales"))
            .and(query_param("sessionId", "gea-session-1"))
            .and(query_param("conversationId", "conversation-1"))
            .and(header("x-delegation-token", "delegation-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "revision": "pending-r1",
                    "items": [{
                        "id": "erp:cost-center:payment-1",
                        "version": "v1",
                        "status": "pending",
                        "kind": "question",
                        "title": "补充成本中心",
                        "sourceLabel": "ERP 财务系统",
                        "allowedActions": ["answer", "decline"],
                        "updatedAt": "2026-08-17T10:00:10+08:00",
                        "presentation": {
                            "type": "question",
                            "questions": [{
                                "question": "本次付款申请应归属哪个成本中心？",
                                "multiSelect": false,
                                "options": [{ "label": "华东业务中心" }]
                            }]
                        }
                    }]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let service = authenticated_service(&server).await;
        create_session(&service).await;
        let snapshot = service
            .list_interaction_requests("user-1", "conversation-1")
            .await
            .unwrap();

        assert_eq!(snapshot.revision, "pending-r1");
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].kind, GeaInteractionRequestKind::Question);
        assert_eq!(snapshot.items[0].status, GeaInteractionRequestStatus::Pending);
    }

    #[tokio::test]
    async fn interaction_action_injects_gateway_context_and_preserves_the_receipt() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("POST"))
            .and(path(
                "/ai/gateway/interaction-requests/erp%3Acost-center%3Apayment-1/actions",
            ))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "sessionId": "gea-session-1",
                "conversationId": "conversation-1",
                "delegationToken": "delegation-secret",
                "expectedVersion": "v1",
                "idempotencyKey": "interaction:erp:cost-center:payment-1:v1:answer",
                "actionId": "answer",
                "payload": {
                    "answers": [{
                        "question": "本次付款申请应归属哪个成本中心？",
                        "labels": ["华东业务中心"]
                    }]
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "receiptId": "receipt-1",
                    "requestId": "erp:cost-center:payment-1",
                    "version": "v1",
                    "status": "accepted",
                    "resolvedAt": "2026-08-17T10:03:00+08:00",
                    "resolvedBy": "user-opaque-id",
                    "auditId": "audit-2"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let service = authenticated_service(&server).await;
        create_session(&service).await;
        let receipt = service
            .act_on_interaction_request(
                "user-1",
                "conversation-1",
                "erp:cost-center:payment-1",
                GeaInteractionRequestActionCommand {
                    expected_version: "v1".to_owned(),
                    idempotency_key: "interaction:erp:cost-center:payment-1:v1:answer".to_owned(),
                    action_id: "answer".to_owned(),
                    payload: Some(json!({
                        "answers": [{
                            "question": "本次付款申请应归属哪个成本中心？",
                            "labels": ["华东业务中心"]
                        }]
                    })),
                },
            )
            .await
            .unwrap();

        assert_eq!(receipt.receipt_id, "receipt-1");
        assert_eq!(receipt.status, GeaInteractionRequestReceiptStatus::Accepted);
        assert_eq!(receipt.audit_id.as_deref(), Some("audit-2"));
    }
}
