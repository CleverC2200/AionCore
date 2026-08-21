#[cfg(feature = "test-support")]
use std::collections::VecDeque;
use std::sync::Arc;

use aionui_api_types::{
    ApprovalActionReceipt, ApprovalActionReceiptStatus, ApprovalComment, ApprovalContact, ApprovalFormField,
    ApprovalInstance, ApprovalInstanceTask, ApprovalList, ApprovalListTopic, ApprovalNode, ApprovalNodeApprover,
    ApprovalOperation, ApprovalSummary, ApprovalTask, ApprovalTaskActionRequest, ApprovalTaskTransferRequest,
};
use aionui_db::{CreateApprovalActionIntentParams, IApprovalReceiptRepository};
use aionui_runtime::Builder;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, RwLock};

use crate::ApprovalError;

#[derive(Debug)]
enum RunError {
    Unavailable,
    Ambiguous,
    Upstream { code: Option<String> },
}

#[async_trait]
trait CommandRunner: Send + Sync {
    async fn run(&self, args: Vec<String>) -> Result<Value, RunError>;
}

struct LarkCliRunner {
    binary: String,
}

#[async_trait]
impl CommandRunner for LarkCliRunner {
    async fn run(&self, args: Vec<String>) -> Result<Value, RunError> {
        let mut command = Builder::clean_cli(&self.binary);
        command.args(args);
        let output = command.output().await.map_err(|_| RunError::Unavailable)?;
        let candidate = if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        let envelope: Value = serde_json::from_slice(candidate).map_err(|_| RunError::Ambiguous)?;
        if output.status.success() && envelope.get("ok").and_then(Value::as_bool) == Some(true) {
            return envelope.get("data").cloned().ok_or(RunError::Ambiguous);
        }
        let error = envelope.get("error").unwrap_or(&envelope);
        Err(RunError::Upstream {
            code: string_value(error.get("code")),
        })
    }
}

#[cfg(feature = "test-support")]
struct TestCommandRunner {
    responses: Mutex<VecDeque<Value>>,
}

#[cfg(feature = "test-support")]
#[async_trait]
impl CommandRunner for TestCommandRunner {
    async fn run(&self, _args: Vec<String>) -> Result<Value, RunError> {
        self.responses.lock().await.pop_front().ok_or(RunError::Ambiguous)
    }
}

#[derive(Clone)]
pub struct ApprovalService {
    runner: Arc<dyn CommandRunner>,
    receipt_repo: Arc<dyn IApprovalReceiptRepository>,
    write_lock: Arc<Mutex<()>>,
    verified_contacts: Arc<RwLock<std::collections::HashSet<String>>>,
    enabled: bool,
}

impl ApprovalService {
    pub fn from_env(receipt_repo: Arc<dyn IApprovalReceiptRepository>, enabled: bool) -> Self {
        let binary = std::env::var("AIONUI_LARK_CLI_BIN").unwrap_or_else(|_| "lark-cli".to_owned());
        Self::new_with_enabled(Arc::new(LarkCliRunner { binary }), receipt_repo, enabled)
    }

    #[cfg(test)]
    fn new(runner: Arc<dyn CommandRunner>, receipt_repo: Arc<dyn IApprovalReceiptRepository>) -> Self {
        Self::new_with_enabled(runner, receipt_repo, true)
    }

    fn new_with_enabled(
        runner: Arc<dyn CommandRunner>,
        receipt_repo: Arc<dyn IApprovalReceiptRepository>,
        enabled: bool,
    ) -> Self {
        Self {
            runner,
            receipt_repo,
            write_lock: Arc::new(Mutex::new(())),
            verified_contacts: Arc::new(RwLock::new(std::collections::HashSet::new())),
            enabled,
        }
    }

    #[cfg(feature = "test-support")]
    pub fn from_test_responses(
        responses: Vec<Value>,
        receipt_repo: Arc<dyn IApprovalReceiptRepository>,
        enabled: bool,
    ) -> Self {
        Self::new_with_enabled(
            Arc::new(TestCommandRunner {
                responses: Mutex::new(responses.into()),
            }),
            receipt_repo,
            enabled,
        )
    }

    pub async fn list_tasks(
        &self,
        topic: ApprovalListTopic,
        page_size: u16,
        definition_code: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<ApprovalList, ApprovalError> {
        self.ensure_enabled()?;
        let mut args = vec![
            "approval".to_owned(),
            "tasks".to_owned(),
            "query".to_owned(),
            "--topic".to_owned(),
            topic.as_feishu_topic().to_owned(),
            "--page-size".to_owned(),
            page_size.to_string(),
            "--as".to_owned(),
            "user".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ];
        if let Some(code) = definition_code.filter(|value| !value.trim().is_empty()) {
            validate_identifier(code, "审批定义 Code")?;
            args.extend(["--definition-code".to_owned(), code.to_owned()]);
        }
        if let Some(token) = page_token.filter(|value| !value.trim().is_empty()) {
            args.extend(["--page-token".to_owned(), token.to_owned()]);
        }
        let data = self.run_read(args).await?;
        parse_task_list(&data)
    }

    pub async fn get_instance(&self, instance_code: &str) -> Result<ApprovalInstance, ApprovalError> {
        self.ensure_enabled()?;
        validate_identifier(instance_code, "审批实例 Code")?;
        let args = vec![
            "approval".to_owned(),
            "instances".to_owned(),
            "get".to_owned(),
            "--instance-code".to_owned(),
            instance_code.to_owned(),
            "--as".to_owned(),
            "user".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ];
        let data = self.run_read(args).await?;
        let mut instance = parse_instance(&data)?;
        if let Err(error) = self.resolve_instance_user_names(&mut instance).await {
            tracing::warn!(error = ?error, "failed to resolve Feishu approval user names");
        }
        Ok(instance)
    }

    async fn resolve_instance_user_names(&self, instance: &mut ApprovalInstance) -> Result<(), ApprovalError> {
        let mut user_ids = std::collections::BTreeSet::new();
        user_ids.extend(instance.tasks.iter().map(|task| task.user_id.clone()));
        user_ids.extend(
            instance
                .operations
                .iter()
                .filter_map(|operation| operation.user_id.clone()),
        );
        user_ids.extend(instance.comments.iter().map(|comment| comment.user_id.clone()));
        if user_ids.is_empty() {
            return Ok(());
        }

        let user_ids: Vec<_> = user_ids.into_iter().collect();
        let mut names = std::collections::HashMap::new();
        for chunk in user_ids.chunks(30) {
            let args = vec![
                "contact".to_owned(),
                "+search-user".to_owned(),
                "--user-ids".to_owned(),
                chunk.join(","),
                "--page-size".to_owned(),
                "30".to_owned(),
                "--lang".to_owned(),
                "zh_cn".to_owned(),
                "--as".to_owned(),
                "user".to_owned(),
                "--format".to_owned(),
                "json".to_owned(),
            ];
            let data = self.run_read(args).await?;
            names.extend(
                data.get("users")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_contact)
                    .map(|contact| (contact.open_id, contact.name)),
            );
        }

        for task in &mut instance.tasks {
            task.user_name = names.get(&task.user_id).cloned();
        }
        for operation in &mut instance.operations {
            operation.user_name = operation
                .user_id
                .as_ref()
                .and_then(|user_id| names.get(user_id).cloned());
        }
        for comment in &mut instance.comments {
            comment.user_name = names.get(&comment.user_id).cloned();
        }
        Ok(())
    }

    pub async fn search_contacts(&self, query: &str) -> Result<Vec<ApprovalContact>, ApprovalError> {
        self.ensure_enabled()?;
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 50 {
            return Err(ApprovalError::invalid("联系人关键词无效"));
        }
        let args = vec![
            "contact".to_owned(),
            "+search-user".to_owned(),
            "--query".to_owned(),
            query.to_owned(),
            "--page-size".to_owned(),
            "20".to_owned(),
            "--exclude-external-users".to_owned(),
            "--as".to_owned(),
            "user".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ];
        let data = self.run_read(args).await?;
        let contacts: Vec<_> = data
            .get("users")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_contact)
            .filter(|contact| !contact.is_cross_tenant)
            .collect();
        self.verified_contacts
            .write()
            .await
            .extend(contacts.iter().map(|contact| contact.open_id.clone()));
        Ok(contacts)
    }

    pub async fn approve(&self, request: ApprovalTaskActionRequest) -> Result<ApprovalActionReceipt, ApprovalError> {
        self.ensure_enabled()?;
        validate_action(&request.instance_code, &request.task_id, &request.idempotency_key)?;
        let data = json!({
            "instance_code": request.instance_code,
            "task_id": request.task_id,
            "comment": request.comment.and_then(non_empty),
        });
        self.run_write(
            "approve",
            data,
            request.instance_code,
            request.task_id,
            request.idempotency_key,
        )
        .await
    }

    pub async fn reject(&self, request: ApprovalTaskActionRequest) -> Result<ApprovalActionReceipt, ApprovalError> {
        self.ensure_enabled()?;
        validate_action(&request.instance_code, &request.task_id, &request.idempotency_key)?;
        let data = json!({
            "instance_code": request.instance_code,
            "task_id": request.task_id,
            "comment": request.comment.and_then(non_empty),
        });
        self.run_write(
            "reject",
            data,
            request.instance_code,
            request.task_id,
            request.idempotency_key,
        )
        .await
    }

    pub async fn transfer(&self, request: ApprovalTaskTransferRequest) -> Result<ApprovalActionReceipt, ApprovalError> {
        self.ensure_enabled()?;
        validate_action(&request.instance_code, &request.task_id, &request.idempotency_key)?;
        validate_identifier(&request.transfer_user_id, "转交人 open_id")?;
        if !self.verified_contacts.read().await.contains(&request.transfer_user_id) {
            return Err(ApprovalError::invalid("转交人未通过当前飞书联系人查询验证"));
        }
        let data = json!({
            "instance_code": request.instance_code,
            "task_id": request.task_id,
            "transfer_user_id": request.transfer_user_id,
            "comment": request.comment.and_then(non_empty),
        });
        self.run_write(
            "transfer",
            data,
            request.instance_code,
            request.task_id,
            request.idempotency_key,
        )
        .await
    }

    fn ensure_enabled(&self) -> Result<(), ApprovalError> {
        if !self.enabled {
            return Err(ApprovalError::TrustedClientRequired);
        }
        Ok(())
    }

    async fn run_read(&self, args: Vec<String>) -> Result<Value, ApprovalError> {
        self.runner.run(args).await.map_err(|error| match error {
            RunError::Unavailable => ApprovalError::ProviderUnavailable,
            RunError::Ambiguous => ApprovalError::InvalidProviderResponse,
            RunError::Upstream { code } => ApprovalError::upstream(code.as_deref()),
        })
    }

    async fn run_write(
        &self,
        action: &str,
        data: Value,
        instance_code: String,
        task_id: String,
        idempotency_key: String,
    ) -> Result<ApprovalActionReceipt, ApprovalError> {
        let _write_guard = self.write_lock.lock().await;
        let payload = data.to_string();
        if let Some(stored) = self.receipt_repo.load(&idempotency_key).await.map_err(storage_error)? {
            if stored.action != action
                || stored.payload != payload
                || stored.instance_code != instance_code
                || stored.task_id != task_id
            {
                return Err(ApprovalError::IdempotencyConflict);
            }
            return stored.receipt.map_or_else(
                || {
                    Ok(ApprovalActionReceipt {
                        status: ApprovalActionReceiptStatus::UnknownExternalWrite,
                        instance_code,
                        task_id,
                        idempotency_key,
                    })
                },
                |receipt| serde_json::from_str(&receipt).map_err(|_| ApprovalError::StorageUnavailable),
            );
        }
        self.receipt_repo
            .create_intent(&CreateApprovalActionIntentParams {
                idempotency_key: idempotency_key.clone(),
                action: action.to_owned(),
                payload: payload.clone(),
                instance_code: instance_code.clone(),
                task_id: task_id.clone(),
            })
            .await
            .map_err(storage_error)?;
        let mut args = vec!["approval".to_owned(), "tasks".to_owned(), action.to_owned()];
        if action == "transfer" {
            args.extend(["--user-id-type".to_owned(), "open_id".to_owned()]);
        }
        args.extend([
            "--data".to_owned(),
            payload,
            "--as".to_owned(),
            "user".to_owned(),
            "--yes".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ]);
        let status = match self.runner.run(args).await {
            Ok(_) => ApprovalActionReceiptStatus::Succeeded,
            Err(RunError::Unavailable | RunError::Ambiguous) => ApprovalActionReceiptStatus::UnknownExternalWrite,
            Err(RunError::Upstream { code }) => {
                self.receipt_repo
                    .delete_intent(&idempotency_key)
                    .await
                    .map_err(storage_error)?;
                return Err(ApprovalError::upstream(code.as_deref()));
            }
        };
        let mut receipt = ApprovalActionReceipt {
            status,
            instance_code,
            task_id,
            idempotency_key: idempotency_key.clone(),
        };
        let encoded = serde_json::to_string(&receipt).map_err(|_| ApprovalError::StorageUnavailable)?;
        if let Err(error) = self.receipt_repo.store_receipt(&idempotency_key, &encoded).await {
            tracing::error!(error = %error, "failed to persist Feishu approval action receipt");
            receipt.status = ApprovalActionReceiptStatus::UnknownExternalWrite;
        }
        Ok(receipt)
    }
}

fn storage_error(error: aionui_db::DbError) -> ApprovalError {
    tracing::error!(error = %error, "Feishu approval receipt storage failed");
    ApprovalError::StorageUnavailable
}

fn parse_task_list(data: &Value) -> Result<ApprovalList, ApprovalError> {
    let object = object(data)?;
    let tasks = object
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(parse_error)?
        .iter()
        .map(parse_task)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ApprovalList {
        count: object
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(tasks.len() as u64),
        has_more: object.get("has_more").and_then(Value::as_bool).unwrap_or(false),
        page_token: string_value(object.get("page_token")),
        tasks,
    })
}

fn parse_task(value: &Value) -> Result<ApprovalTask, ApprovalError> {
    let value = object(value)?;
    Ok(ApprovalTask {
        task_id: required_string(value, "task_id")?,
        instance_code: required_string(value, "instance_code")?,
        definition_code: required_string(value, "definition_code")?,
        definition_name: required_string(value, "definition_name")?,
        title: required_string(value, "title")?,
        topic: required_string(value, "topic")?,
        status: required_string(value, "status")?,
        instance_status: required_string(value, "instance_status")?,
        initiator_id: string_value(value.get("initiator")),
        initiator_name: string_value(value.get("initiator_name")),
        user_id: required_string(value, "user_id")?,
        support_api_operate: value
            .get("support_api_operate")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        link: string_value(value.get("link")),
        summaries: value
            .get("summaries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|summary| {
                let summary = summary.as_object()?;
                Some(ApprovalSummary {
                    key: string_value(summary.get("key"))?,
                    value: string_value(summary.get("value"))?,
                })
            })
            .collect(),
    })
}

fn parse_instance(data: &Value) -> Result<ApprovalInstance, ApprovalError> {
    let value = object(data)?;
    let form_json = required_string(value, "form")?;
    let form_values: Vec<Value> = serde_json::from_str(&form_json).map_err(|_| parse_error())?;
    Ok(ApprovalInstance {
        instance_code: required_string(value, "instance_code")?,
        definition_code: required_string(value, "definition_code")?,
        definition_name: required_string(value, "definition_name")?,
        serial_number: required_string(value, "serial_number")?,
        status: required_string(value, "status")?,
        start_time: required_string(value, "start_time")?,
        end_time: required_string(value, "end_time")?,
        initiator_id: required_string(value, "user_id")?,
        department_id: string_value(value.get("department_id")),
        form: form_values.iter().filter_map(parse_form_field).collect(),
        current_nodes: value
            .get("current_nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_node)
            .collect(),
        tasks: value
            .get("tasks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_instance_task)
            .collect(),
        operations: value
            .get("operation_records")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_operation)
            .collect(),
        comments: value
            .get("comments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_comment)
            .collect(),
    })
}

fn parse_form_field(value: &Value) -> Option<ApprovalFormField> {
    let value = value.as_object()?;
    Some(ApprovalFormField {
        id: string_value(value.get("id"))?,
        custom_id: string_value(value.get("custom_id")),
        name: string_value(value.get("name"))?,
        field_type: string_value(value.get("type"))?,
        value: value.get("value").cloned().unwrap_or(Value::Null),
    })
}

fn parse_contact(value: &Value) -> Option<ApprovalContact> {
    let value = value.as_object()?;
    Some(ApprovalContact {
        open_id: string_value(value.get("open_id"))?,
        name: string_value(value.get("localized_name"))?,
        department: string_value(value.get("department")),
        enterprise_email: string_value(value.get("enterprise_email")),
        is_cross_tenant: value.get("is_cross_tenant").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn parse_node(value: &Value) -> Option<ApprovalNode> {
    let value = value.as_object()?;
    Some(ApprovalNode {
        node_id: string_value(value.get("node_id")),
        node_name: string_value(value.get("node_name")),
        node_type: string_value(value.get("type")),
        approvers: value
            .get("approvers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|approver| {
                let approver = approver.as_object()?;
                Some(ApprovalNodeApprover {
                    task_id: string_value(approver.get("task_id")),
                    user_id: string_value(approver.get("user_id")),
                })
            })
            .collect(),
    })
}

fn parse_operation(value: &Value) -> Option<ApprovalOperation> {
    let value = value.as_object()?;
    Some(ApprovalOperation {
        operation_type: string_value(value.get("type"))?,
        create_time: string_value(value.get("create_time"))?,
        user_id: string_value(value.get("user_id")),
        user_name: None,
        task_id: string_value(value.get("task_id")),
        node_id: string_value(value.get("node_id")),
        comment: string_value(value.get("comment")),
    })
}

fn parse_instance_task(value: &Value) -> Option<ApprovalInstanceTask> {
    let value = value.as_object()?;
    Some(ApprovalInstanceTask {
        id: string_value(value.get("id"))?,
        user_id: string_value(value.get("user_id"))?,
        user_name: None,
        node_id: string_value(value.get("node_id")),
        node_name: string_value(value.get("node_name")),
        status: string_value(value.get("status"))?,
        task_type: string_value(value.get("type")),
        start_time: string_value(value.get("start_time"))?,
        end_time: string_value(value.get("end_time")).unwrap_or_else(|| "0".to_owned()),
    })
}

fn parse_comment(value: &Value) -> Option<ApprovalComment> {
    let value = value.as_object()?;
    Some(ApprovalComment {
        id: string_value(value.get("id"))?,
        user_id: string_value(value.get("user_id"))?,
        user_name: None,
        create_time: string_value(value.get("create_time"))?,
        comment: string_value(value.get("comment"))?,
    })
}

fn object(value: &Value) -> Result<&Map<String, Value>, ApprovalError> {
    value.as_object().ok_or_else(parse_error)
}

fn required_string(value: &Map<String, Value>, key: &str) -> Result<String, ApprovalError> {
    string_value(value.get(key)).ok_or_else(parse_error)
}

fn string_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn validate_action(instance_code: &str, task_id: &str, idempotency_key: &str) -> Result<(), ApprovalError> {
    validate_identifier(instance_code, "审批实例 Code")?;
    validate_identifier(task_id, "审批任务 ID")?;
    validate_identifier(idempotency_key, "幂等键")
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ApprovalError> {
    if value.trim().is_empty() || value.len() > 256 {
        return Err(ApprovalError::invalid(format!("{label} 无效")));
    }
    Ok(())
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn parse_error() -> ApprovalError {
    ApprovalError::InvalidProviderResponse
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use aionui_db::{CreateApprovalActionIntentParams, DbError, StoredApprovalActionReceipt};

    #[derive(Default)]
    struct MemoryReceiptRepository {
        rows: Mutex<HashMap<String, StoredApprovalActionReceipt>>,
    }

    #[async_trait]
    impl IApprovalReceiptRepository for MemoryReceiptRepository {
        async fn load(&self, idempotency_key: &str) -> Result<Option<StoredApprovalActionReceipt>, DbError> {
            Ok(self.rows.lock().await.get(idempotency_key).cloned())
        }

        async fn create_intent(&self, params: &CreateApprovalActionIntentParams) -> Result<(), DbError> {
            self.rows.lock().await.insert(
                params.idempotency_key.clone(),
                StoredApprovalActionReceipt {
                    idempotency_key: params.idempotency_key.clone(),
                    action: params.action.clone(),
                    payload: params.payload.clone(),
                    instance_code: params.instance_code.clone(),
                    task_id: params.task_id.clone(),
                    receipt: None,
                },
            );
            Ok(())
        }

        async fn store_receipt(&self, idempotency_key: &str, receipt: &str) -> Result<(), DbError> {
            self.rows.lock().await.get_mut(idempotency_key).unwrap().receipt = Some(receipt.to_owned());
            Ok(())
        }

        async fn delete_intent(&self, idempotency_key: &str) -> Result<(), DbError> {
            self.rows.lock().await.remove(idempotency_key);
            Ok(())
        }
    }

    fn test_service(runner: Arc<dyn CommandRunner>) -> ApprovalService {
        ApprovalService::new(runner, Arc::new(MemoryReceiptRepository::default()))
    }

    struct FakeRunner {
        calls: AtomicUsize,
        result: Mutex<Option<Result<Value, RunError>>>,
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, _args: Vec<String>) -> Result<Value, RunError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.lock().await.take().unwrap_or(Ok(json!({})))
        }
    }

    struct CapturingRunner {
        args: Mutex<Vec<String>>,
        result: Value,
    }

    struct SequenceRunner {
        args: Mutex<Vec<Vec<String>>>,
        results: Mutex<VecDeque<Value>>,
    }

    #[async_trait]
    impl CommandRunner for SequenceRunner {
        async fn run(&self, args: Vec<String>) -> Result<Value, RunError> {
            self.args.lock().await.push(args);
            self.results.lock().await.pop_front().ok_or(RunError::Ambiguous)
        }
    }

    #[async_trait]
    impl CommandRunner for CapturingRunner {
        async fn run(&self, args: Vec<String>) -> Result<Value, RunError> {
            *self.args.lock().await = args;
            Ok(self.result.clone())
        }
    }

    #[test]
    fn task_parser_accepts_numeric_feishu_statuses() {
        let parsed = parse_task_list(&json!({
            "count": 1,
            "has_more": false,
            "tasks": [{
                "task_id": "task-1",
                "instance_code": "instance-1",
                "definition_code": "definition-1",
                "definition_name": "需求预测测试",
                "title": "需求预测测试",
                "topic": 1,
                "status": 1,
                "instance_status": 1,
                "user_id": "ou_owner",
                "support_api_operate": true,
                "summaries": [{"key": "事项说明", "value": "计划提报"}]
            }]
        }))
        .expect("task list");
        assert_eq!(parsed.tasks[0].status, "1");
        assert!(parsed.tasks[0].support_api_operate);
    }

    #[tokio::test]
    async fn task_query_forwards_the_definition_filter_to_lark_cli() {
        let runner = Arc::new(CapturingRunner {
            args: Mutex::new(Vec::new()),
            result: json!({ "count": 0, "has_more": false, "tasks": [] }),
        });
        let service = test_service(runner.clone());

        service
            .list_tasks(
                ApprovalListTopic::Done,
                100,
                Some("1DA97CD8-B406-4A76-A39E-CFCB5AFEBB60"),
                None,
            )
            .await
            .expect("filtered list");

        let args = runner.args.lock().await;
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--definition-code", "1DA97CD8-B406-4A76-A39E-CFCB5AFEBB60",] })
        );
    }

    #[test]
    fn instance_parser_keeps_generic_fields_tasks_comments_and_attachment_references() {
        let parsed = parse_instance(&json!({
            "instance_code": "instance-1",
            "definition_code": "definition-1",
            "definition_name": "需求预测测试",
            "serial_number": "202608200032",
            "status": "PENDING",
            "start_time": "1787184000000",
            "end_time": "0",
            "user_id": "ou_initiator",
            "form": serde_json::to_string(&json!([{
                "id": "attachment",
                "name": "附件",
                "type": "attachmentV2",
                "value": ["https://example.invalid/file.md"]
            }])).unwrap(),
            "current_nodes": [],
            "tasks": [{
                "id": "task-1",
                "user_id": "ou_owner",
                "node_id": "node-1",
                "node_name": "审批人1",
                "status": "PENDING",
                "type": "SEQUENTIAL",
                "start_time": "1787184000000",
                "end_time": "0"
            }],
            "operation_records": [{"type": "START", "create_time": "1787184000000"}],
            "comments": [{
                "id": "comment-1",
                "user_id": "ou_owner",
                "create_time": "1787184000001",
                "comment": "请核对"
            }]
        }))
        .expect("instance");
        assert_eq!(parsed.form[0].field_type, "attachmentV2");
        assert_eq!(parsed.tasks[0].status, "PENDING");
        assert_eq!(parsed.comments[0].comment, "请核对");
    }

    #[tokio::test]
    async fn instance_users_are_resolved_to_localized_contact_names_in_one_batch() {
        let runner = Arc::new(SequenceRunner {
            args: Mutex::new(Vec::new()),
            results: Mutex::new(
                vec![
                    json!({
                        "instance_code": "instance-1",
                        "definition_code": "definition-1",
                        "definition_name": "需求预测测试",
                        "serial_number": "202608200032",
                        "status": "APPROVED",
                        "start_time": "1787184000000",
                        "end_time": "1787187600000",
                        "user_id": "ou_initiator",
                        "form": "[]",
                        "current_nodes": [],
                        "tasks": [{
                            "id": "task-1",
                            "user_id": "ou_owner",
                            "status": "APPROVED",
                            "start_time": "1787184000000",
                            "end_time": "1787187600000"
                        }],
                        "operation_records": [{
                            "type": "START",
                            "create_time": "1787184000000",
                            "user_id": "ou_initiator"
                        }],
                        "comments": [{
                            "id": "comment-1",
                            "user_id": "ou_owner",
                            "create_time": "1787184000001",
                            "comment": "请核对"
                        }]
                    }),
                    json!({
                        "users": [
                            {"open_id": "ou_initiator", "localized_name": "陈发起", "is_cross_tenant": false},
                            {"open_id": "ou_owner", "localized_name": "王审批", "is_cross_tenant": false}
                        ]
                    }),
                ]
                .into(),
            ),
        });
        let service = test_service(runner.clone());

        let instance = service.get_instance("instance-1").await.expect("resolved instance");

        assert_eq!(instance.tasks[0].user_name.as_deref(), Some("王审批"));
        assert_eq!(instance.operations[0].user_name.as_deref(), Some("陈发起"));
        assert_eq!(instance.comments[0].user_name.as_deref(), Some("王审批"));
        let calls = runner.args.lock().await;
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1]
                .windows(2)
                .any(|pair| pair == ["--user-ids", "ou_initiator,ou_owner"])
        );
    }

    #[tokio::test]
    async fn idempotency_key_prevents_second_external_write() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Ok(json!({})))),
        });
        let service = test_service(runner.clone());
        let request = ApprovalTaskActionRequest {
            instance_code: "instance-1".to_owned(),
            task_id: "task-1".to_owned(),
            comment: None,
            idempotency_key: "intent-1".to_owned(),
        };
        service.approve(request.clone()).await.expect("first write");
        service.approve(request).await.expect("cached write");
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_same_key_writes_are_coalesced() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Ok(json!({})))),
        });
        let service = test_service(runner.clone());
        let request = ApprovalTaskActionRequest {
            instance_code: "instance-1".to_owned(),
            task_id: "task-1".to_owned(),
            comment: None,
            idempotency_key: "intent-concurrent".to_owned(),
        };
        let (first, second) = tokio::join!(service.approve(request.clone()), service.approve(request));
        assert_eq!(first.expect("first"), second.expect("second"));
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn same_key_cannot_be_reused_for_a_different_action_target() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Ok(json!({})))),
        });
        let service = test_service(runner.clone());
        service
            .approve(ApprovalTaskActionRequest {
                instance_code: "instance-1".to_owned(),
                task_id: "task-1".to_owned(),
                comment: None,
                idempotency_key: "intent-reused".to_owned(),
            })
            .await
            .expect("first write");
        let error = service
            .approve(ApprovalTaskActionRequest {
                instance_code: "instance-2".to_owned(),
                task_id: "task-2".to_owned(),
                comment: None,
                idempotency_key: "intent-reused".to_owned(),
            })
            .await
            .expect_err("reused key must conflict");
        assert_eq!(error, ApprovalError::IdempotencyConflict);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ambiguous_write_is_returned_as_unknown_and_never_retried() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Err(RunError::Ambiguous))),
        });
        let service = test_service(runner.clone());
        let request = ApprovalTaskActionRequest {
            instance_code: "instance-1".to_owned(),
            task_id: "task-1".to_owned(),
            comment: None,
            idempotency_key: "intent-unknown".to_owned(),
        };
        let first = service.approve(request.clone()).await.expect("unknown receipt");
        let second = service.approve(request).await.expect("cached unknown receipt");
        assert_eq!(first.status, ApprovalActionReceiptStatus::UnknownExternalWrite);
        assert_eq!(second, first);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pending_intent_survives_service_restart_without_a_second_external_write() {
        let receipt_repo = Arc::new(MemoryReceiptRepository::default());
        let first_runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Err(RunError::Ambiguous))),
        });
        let request = ApprovalTaskActionRequest {
            instance_code: "instance-1".to_owned(),
            task_id: "task-1".to_owned(),
            comment: None,
            idempotency_key: "intent-restart".to_owned(),
        };
        let first_service = ApprovalService::new(first_runner.clone(), receipt_repo.clone());
        let first = first_service.approve(request.clone()).await.expect("unknown receipt");
        assert_eq!(first.status, ApprovalActionReceiptStatus::UnknownExternalWrite);

        let restarted_runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Ok(json!({})))),
        });
        let restarted_service = ApprovalService::new(restarted_runner.clone(), receipt_repo);
        let replay = restarted_service.approve(request).await.expect("durable replay");
        assert_eq!(replay.status, ApprovalActionReceiptStatus::UnknownExternalWrite);
        assert_eq!(first_runner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(restarted_runner.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stale_task_is_a_conflict_without_leaking_cli_error_text() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Err(RunError::Upstream {
                code: Some("1395001".to_owned()),
            }))),
        });
        let service = test_service(runner);
        let error = service
            .approve(ApprovalTaskActionRequest {
                instance_code: "instance-1".to_owned(),
                task_id: "task-1".to_owned(),
                comment: None,
                idempotency_key: "intent-conflict".to_owned(),
            })
            .await
            .expect_err("stale task must fail");
        assert_eq!(error, ApprovalError::Upstream(crate::ApprovalUpstreamError::StaleTask));
    }

    #[tokio::test]
    async fn transfer_rejects_an_open_id_not_returned_by_contact_search() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Ok(json!({})))),
        });
        let service = test_service(runner.clone());
        let error = service
            .transfer(ApprovalTaskTransferRequest {
                instance_code: "instance-1".to_owned(),
                task_id: "task-1".to_owned(),
                transfer_user_id: "ou_unverified".to_owned(),
                comment: None,
                idempotency_key: "intent-transfer".to_owned(),
            })
            .await
            .expect_err("unverified recipient must fail");
        assert!(matches!(error, ApprovalError::Invalid(_)));
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn non_local_identity_cannot_access_the_machine_lark_profile() {
        let runner = Arc::new(FakeRunner {
            calls: AtomicUsize::new(0),
            result: Mutex::new(Some(Ok(json!({})))),
        });
        let service =
            ApprovalService::new_with_enabled(runner.clone(), Arc::new(MemoryReceiptRepository::default()), false);
        let error = service
            .list_tasks(ApprovalListTopic::Pending, 10, None, None)
            .await
            .expect_err("must reject non-local access");
        assert_eq!(error, ApprovalError::TrustedClientRequired);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }
}
