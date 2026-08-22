use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GeaClientResourceKind {
    Assistants,
    Skills,
    Mcps,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SyncGeaClientResourcesRequest {
    pub resources: Vec<GeaClientResourceKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum GeaClientResourceSyncStatus {
    Completed,
    NotAuthenticated,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaClientResourceSyncResult {
    pub changed: usize,
    pub failed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub skipped: usize,
    pub status: GeaClientResourceSyncStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportGeaSkillExecutionRequest {
    pub skill_code: String,
    pub version: String,
    pub digest: String,
    pub success: bool,
    pub executed_at: String,
    pub duration_ms: u64,
    pub result_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaResourceCatalogEnvelope {
    pub status: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default, alias = "last_good_revision")]
    pub last_good_revision: Option<String>,
    #[serde(
        default,
        alias = "server_time",
        deserialize_with = "deserialize_optional_string_or_number"
    )]
    pub server_time: Option<String>,
    #[serde(default)]
    pub snapshot: Option<GeaResourceCatalogSnapshot>,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaResourceCatalogSnapshot {
    #[serde(alias = "schema_version")]
    pub schema_version: u32,
    pub revision: String,
    #[serde(default, alias = "generated_at")]
    pub generated_at: Option<String>,
    #[serde(default, alias = "tenant_id")]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub skills: Vec<GeaCatalogSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaCatalogSkill {
    #[serde(alias = "skillCode", alias = "skill_code")]
    pub id: String,
    pub version: String,
    pub name: GeaLocalizedText,
    #[serde(default)]
    pub description: GeaLocalizedText,
    #[serde(alias = "artifact_ref")]
    pub artifact_ref: String,
    pub digest: String,
    #[serde(alias = "sizeBytes", alias = "size_bytes")]
    pub artifact_size: u64,
    pub state: String,
    #[serde(default, alias = "risk_level")]
    pub risk_level: Option<String>,
    #[serde(default, alias = "minimum_client_version")]
    pub minimum_client_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GeaLocalizedText {
    Plain(String),
    Localized {
        default: String,
        #[serde(default)]
        translations: std::collections::HashMap<String, String>,
    },
}

impl Default for GeaLocalizedText {
    fn default() -> Self {
        Self::Plain(String::new())
    }
}

fn deserialize_optional_string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(Value::Number(value)) => Ok(Some(value.to_string())),
        Some(_) => Err(D::Error::custom("expected a string or number")),
    }
}

impl GeaLocalizedText {
    pub fn display_value(&self) -> &str {
        match self {
            Self::Plain(value) => value,
            Self::Localized { default, .. } => default,
        }
    }
}

/// Transfers an already authenticated GEA login session from the trusted
/// desktop process into AionCore. The token is accepted only at this boundary
/// and is never returned by any response.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetGeaAuthSessionRequest {
    pub access_token: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaAuthSessionStatus {
    pub authenticated: bool,
    pub reauth_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateGeaSessionRequest {
    pub consumer_code: String,
    #[serde(default)]
    pub preparation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaSessionResponse {
    pub session_id: String,
    pub conversation_id: String,
    pub consumer_code: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective_capability_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaToolInfo {
    pub name: String,
    pub source_code: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaToolCallRequest {
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaToolCallResponse {
    pub result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionRequestKind {
    Question,
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionRequestStatus {
    Pending,
    Processing,
    Resolved,
    Expired,
    Cancelled,
    VerificationRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionQuestion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default)]
    pub multi_select: bool,
    pub options: Vec<GeaInteractionQuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionPermissionOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GeaInteractionPresentation {
    Question {
        #[serde(default)]
        questions: Vec<GeaInteractionQuestion>,
    },
    Permission {
        #[serde(default)]
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default)]
        options: Vec<GeaInteractionPermissionOption>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequest {
    #[serde(rename = "requestId", alias = "id")]
    pub id: String,
    pub version: String,
    pub status: GeaInteractionRequestStatus,
    pub kind: GeaInteractionRequestKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_label: Option<String>,
    pub allowed_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub updated_at: String,
    pub presentation: GeaInteractionPresentation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequestSnapshot {
    pub revision: String,
    pub items: Vec<GeaInteractionRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequestActionCommand {
    pub expected_version: String,
    pub idempotency_key: String,
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionRequestReceiptStatus {
    Processing,
    Accepted,
    Failed,
    AlreadyResolved,
    Conflict,
    Expired,
    Forbidden,
    Cancelled,
    UnknownExternalWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionTurnContinuation {
    OriginalToolCallReleased,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequestReceipt {
    pub receipt_id: String,
    pub request_id: String,
    pub version: String,
    pub status: GeaInteractionRequestReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_continuation: Option<GeaInteractionTurnContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<GeaInteractionRequest>,
}

/// AionCore's recoverable, user-scoped projection returned to AionUi.
///
/// The GEA-owned request above deliberately contains only source data. This
/// view adds local navigation anchors without sending those anchors upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InteractionRequestSource {
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InteractionRequestView {
    pub id: String,
    pub version: String,
    pub kind: GeaInteractionRequestKind,
    pub status: GeaInteractionRequestStatus,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub source: InteractionRequestSource,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub allowed_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InteractionRequestSyncState {
    Complete,
    Partial,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InteractionRequestList {
    pub revision: String,
    pub items: Vec<InteractionRequestView>,
    pub sync_state: InteractionRequestSyncState,
    pub failed_session_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_codes: Vec<String>,
}

/// User-scoped invalidation event for the recoverable interaction-request
/// projection. Clients refetch the complete pending snapshot on receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InteractionRequestChangedPayload {
    pub user_id: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InteractionRequestActionCommand {
    pub expected_version: String,
    pub idempotency_key: String,
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InteractionRequestReceipt {
    pub receipt_id: String,
    pub request_id: String,
    pub version: String,
    pub status: GeaInteractionRequestReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_continuation: Option<GeaInteractionTurnContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<InteractionRequestView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Message,
    Event,
    Reminder,
    ActionRequired,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSeverity {
    Info,
    Success,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    Unread,
    Read,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationTarget {
    Notification,
    Conversation {
        #[serde(rename = "conversationId")]
        conversation_id: String,
    },
    Message {
        #[serde(rename = "conversationId")]
        conversation_id: String,
        #[serde(rename = "messageId")]
        message_id: String,
    },
    Team {
        #[serde(rename = "teamId")]
        team_id: String,
    },
    Slot {
        #[serde(rename = "teamId")]
        team_id: String,
        #[serde(rename = "slotId")]
        slot_id: String,
    },
    InteractionRequest {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(default, rename = "conversationId", skip_serializing_if = "Option::is_none")]
        conversation_id: Option<String>,
        #[serde(default, rename = "teamId", skip_serializing_if = "Option::is_none")]
        team_id: Option<String>,
        #[serde(default, rename = "slotId", skip_serializing_if = "Option::is_none")]
        slot_id: Option<String>,
    },
}

/// GEA-owned notification representation at the upstream seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaNotification {
    #[serde(rename = "notificationId", alias = "id")]
    pub id: String,
    pub version: String,
    pub status: NotificationStatus,
    pub kind: NotificationKind,
    pub severity: NotificationSeverity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub dismissible: bool,
    pub source: String,
    pub target: NotificationTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_request_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaNotificationSnapshot {
    pub revision: String,
    pub items: Vec<GeaNotification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSyncState {
    Idle,
    Syncing,
    Fresh,
    Stale,
    Partial,
    Failed,
}

/// AionCore's tenant-scoped, recoverable projection returned to AionUi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationView {
    pub id: String,
    pub version: String,
    pub status: NotificationStatus,
    pub kind: NotificationKind,
    pub severity: NotificationSeverity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub dismissible: bool,
    pub source: String,
    pub target: NotificationTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_request_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationList {
    pub revision: String,
    pub items: Vec<NotificationView>,
    pub sync_state: NotificationSyncState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationChangedReason {
    Snapshot,
    Created,
    Updated,
    Read,
    Dismissed,
    Expired,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationChangedPayload {
    pub user_id: String,
    pub revision: String,
    pub reason: NotificationChangedReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationActionCommand {
    pub expected_version: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaNotificationReceipt {
    pub receipt_id: String,
    pub notification_id: String,
    pub version: String,
    pub status: NotificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<GeaNotification>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NotificationReceipt {
    pub receipt_id: String,
    pub notification_id: String,
    pub version: String,
    pub status: NotificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<NotificationView>,
}

#[cfg(test)]
mod resource_catalog_tests {
    use super::*;

    #[test]
    fn parses_current_camel_case_resource_catalog() {
        let value = serde_json::json!({
            "status": "ok",
            "revision": "resource-r1",
            "serverTime": "2026-08-18T00:00:00Z",
            "snapshot": {
                "schemaVersion": 1,
                "revision": "resource-r1",
                "skills": [{
                    "id": "sales-forecast",
                    "version": "1.0.0",
                    "name": {"default": "Sales forecast", "translations": {"zh-CN": "销售预测"}},
                    "description": "Query forecast data",
                    "artifactRef": "skill/sales-forecast/1.0.0",
                    "digest": "sha256:abcd",
                    "artifactSize": 123,
                    "state": "active",
                    "riskLevel": "LOW"
                }]
            }
        });
        let parsed: GeaResourceCatalogEnvelope = serde_json::from_value(value).unwrap();
        let skill = &parsed.snapshot.unwrap().skills[0];
        assert_eq!(skill.id, "sales-forecast");
        assert_eq!(skill.artifact_size, 123);
        assert_eq!(skill.name.display_value(), "Sales forecast");
    }

    #[test]
    fn parses_legacy_snake_case_resource_catalog() {
        let value = serde_json::json!({
            "status": "ok",
            "snapshot": {
                "schema_version": 1,
                "revision": "resource-r1",
                "skills": [{
                    "skill_code": "sales-forecast",
                    "version": "1.0.0",
                    "name": "Sales forecast",
                    "description": "Query forecast data",
                    "artifact_ref": "skill/sales-forecast/1.0.0",
                    "digest": "abcd",
                    "size_bytes": 123,
                    "state": "active"
                }]
            }
        });
        let parsed: GeaResourceCatalogEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.snapshot.unwrap().skills[0].id, "sales-forecast");
    }
}
