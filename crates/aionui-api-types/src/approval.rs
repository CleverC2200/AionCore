use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalListTopic {
    Pending,
    Done,
}

impl ApprovalListTopic {
    pub fn as_feishu_topic(self) -> &'static str {
        match self {
            Self::Pending => "1",
            Self::Done => "2",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalSummary {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalTask {
    pub task_id: String,
    pub instance_code: String,
    pub instance_external_id: Option<String>,
    pub task_external_id: Option<String>,
    pub definition_code: String,
    pub definition_name: String,
    pub title: String,
    pub topic: String,
    pub status: String,
    pub instance_status: String,
    pub initiator_id: Option<String>,
    pub initiator_name: Option<String>,
    pub user_id: String,
    pub support_api_operate: bool,
    pub link: Option<String>,
    pub summaries: Vec<ApprovalSummary>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalList {
    pub count: u64,
    pub has_more: bool,
    pub page_token: Option<String>,
    pub tasks: Vec<ApprovalTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalContact {
    pub open_id: String,
    pub name: String,
    pub department: Option<String>,
    pub enterprise_email: Option<String>,
    pub is_cross_tenant: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalFormField {
    pub id: String,
    pub custom_id: Option<String>,
    pub name: String,
    pub field_type: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalNode {
    pub node_id: Option<String>,
    pub node_name: Option<String>,
    pub node_type: Option<String>,
    pub approvers: Vec<ApprovalNodeApprover>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalNodeApprover {
    pub task_id: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalOperation {
    pub operation_type: String,
    pub create_time: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub task_id: Option<String>,
    pub node_id: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalInstanceTask {
    pub id: String,
    pub user_id: String,
    pub user_name: Option<String>,
    pub node_id: Option<String>,
    pub node_name: Option<String>,
    pub status: String,
    pub task_type: Option<String>,
    pub start_time: String,
    pub end_time: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalComment {
    pub id: String,
    pub user_id: String,
    pub user_name: Option<String>,
    pub create_time: String,
    pub comment: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalInstance {
    pub instance_code: String,
    pub definition_code: String,
    pub definition_name: String,
    pub serial_number: String,
    pub status: String,
    pub start_time: String,
    pub end_time: String,
    pub initiator_id: String,
    pub department_id: Option<String>,
    pub form: Vec<ApprovalFormField>,
    pub current_nodes: Vec<ApprovalNode>,
    pub tasks: Vec<ApprovalInstanceTask>,
    pub operations: Vec<ApprovalOperation>,
    pub comments: Vec<ApprovalComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalTaskActionRequest {
    pub instance_code: String,
    pub task_id: String,
    pub comment: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalTaskTransferRequest {
    pub instance_code: String,
    pub task_id: String,
    pub transfer_user_id: String,
    pub comment: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalActionReceiptStatus {
    Succeeded,
    UnknownExternalWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalActionReceipt {
    pub status: ApprovalActionReceiptStatus,
    pub instance_code: String,
    pub task_id: String,
    pub idempotency_key: String,
}
