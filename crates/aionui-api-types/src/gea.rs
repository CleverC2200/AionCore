use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Transfers an already authenticated GEA login session from the trusted
/// desktop process into AionCore. The token is accepted only at this boundary
/// and is never returned by any response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetGeaAuthSessionRequest {
    pub access_token: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaAuthSessionStatus {
    pub authenticated: bool,
    pub reauth_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGeaSessionRequest {
    pub consumer_code: String,
    #[serde(default)]
    pub preparation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaSessionResponse {
    pub session_id: String,
    pub conversation_id: String,
    pub consumer_code: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effective_capability_codes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaToolInfo {
    pub name: String,
    pub source_code: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaToolCallRequest {
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaToolCallResponse {
    pub result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionRequestKind {
    Question,
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionRequestStatus {
    Pending,
    Resolved,
    Expired,
    Cancelled,
    VerificationRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionQuestion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    #[serde(default)]
    pub multi_select: bool,
    pub options: Vec<GeaInteractionQuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionPermissionOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GeaInteractionPresentation {
    Question {
        questions: Vec<GeaInteractionQuestion>,
    },
    Permission {
        title: String,
        description: String,
        operation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        options: Vec<GeaInteractionPermissionOption>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequest {
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
    pub updated_at: String,
    pub presentation: GeaInteractionPresentation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequestSnapshot {
    pub revision: String,
    pub items: Vec<GeaInteractionRequest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequestActionCommand {
    pub expected_version: String,
    pub idempotency_key: String,
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeaInteractionRequestReceiptStatus {
    Accepted,
    AlreadyResolved,
    Conflict,
    Expired,
    Forbidden,
    UnknownExternalWrite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeaInteractionRequestReceipt {
    pub receipt_id: String,
    pub request_id: String,
    pub version: String,
    pub status: GeaInteractionRequestReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<GeaInteractionRequest>,
}
