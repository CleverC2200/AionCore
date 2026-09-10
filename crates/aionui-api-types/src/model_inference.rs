use serde::{Deserialize, Serialize};

/// A bounded text-only inference. Model selection is resolved by the endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInferenceRequest {
    pub question: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ModelInferenceStatus {
    Ok,
    NoAnswer,
    ToolsRequired,
    Timeout,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInferenceResponse {
    pub status: ModelInferenceStatus,
    pub provider_id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}
