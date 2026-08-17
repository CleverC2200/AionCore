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
