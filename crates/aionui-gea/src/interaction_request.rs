use std::collections::HashSet;

use aionui_api_types::{
    GeaInteractionPresentation, GeaInteractionRequest, GeaInteractionRequestActionCommand, GeaInteractionRequestKind,
    GeaInteractionRequestReceipt, GeaInteractionRequestSnapshot, GeaInteractionRequestStatus,
};
use serde_json::Value;

use crate::error::GeaError;

pub(crate) fn validate_action_command(
    request_id: &str,
    command: &GeaInteractionRequestActionCommand,
) -> Result<(), GeaError> {
    for (field, value) in [
        ("requestId", request_id),
        ("expectedVersion", command.expected_version.as_str()),
        ("idempotencyKey", command.idempotency_key.as_str()),
        ("actionId", command.action_id.as_str()),
    ] {
        if value.trim().is_empty() || value.chars().count() > 240 {
            return Err(GeaError::invalid_request(format!("{field} 必须为 1 到 240 个字符")));
        }
    }
    if command.payload.as_ref().is_some_and(|payload| !payload.is_object()) {
        return Err(GeaError::invalid_request("payload 必须是 JSON object"));
    }
    Ok(())
}

pub(crate) fn validate_question_answers(payload: Option<&Value>) -> Result<(), GeaError> {
    let answers = payload
        .and_then(|value| value.get("answers"))
        .and_then(Value::as_array)
        .filter(|answers| !answers.is_empty())
        .ok_or_else(|| GeaError::invalid_request("question 的 answers 必须是非空数组"))?;

    for answer in answers {
        let question = answer
            .get("question")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| GeaError::invalid_request("question 的 answer 必须包含非空 question"))?;
        let labels = answer
            .get("labels")
            .and_then(Value::as_array)
            .filter(|labels| {
                !labels.is_empty()
                    && labels
                        .iter()
                        .all(|label| label.as_str().is_some_and(|v| !v.trim().is_empty()))
            })
            .ok_or_else(|| GeaError::invalid_request("question 的 answer 必须包含非空 labels 数组"))?;
        let _ = (question, labels);
    }
    Ok(())
}

pub(crate) fn parse_snapshot(value: &Value) -> Result<GeaInteractionRequestSnapshot, GeaError> {
    let result = value
        .get("result")
        .ok_or_else(|| invalid_upstream("GEA Interaction Request 响应缺少 result"))?;
    reject_sensitive_fields(result)?;
    let snapshot = serde_json::from_value::<GeaInteractionRequestSnapshot>(result.clone())
        .map_err(|_| invalid_upstream("GEA Interaction Request 快照格式无效"))?;
    validate_identifier("revision", &snapshot.revision)?;
    let mut ids = HashSet::with_capacity(snapshot.items.len());
    for request in &snapshot.items {
        validate_request(request)?;
        if request.status != GeaInteractionRequestStatus::Pending {
            return Err(invalid_upstream("GEA pending 快照包含非 pending 请求"));
        }
        if !ids.insert(request.id.as_str()) {
            return Err(invalid_upstream("GEA pending 快照包含重复 request ID"));
        }
    }
    Ok(snapshot)
}

pub(crate) fn parse_receipt(
    value: &Value,
    expected_request_id: &str,
) -> Result<GeaInteractionRequestReceipt, GeaError> {
    let result = value
        .get("result")
        .ok_or_else(|| invalid_upstream("GEA Interaction Request 动作响应缺少 result"))?;
    reject_sensitive_fields(result)?;
    let receipt = serde_json::from_value::<GeaInteractionRequestReceipt>(result.clone())
        .map_err(|_| invalid_upstream("GEA Interaction Request 回执格式无效"))?;
    validate_identifier("receiptId", &receipt.receipt_id)?;
    validate_identifier("requestId", &receipt.request_id)?;
    validate_identifier("version", &receipt.version)?;
    if receipt.request_id != expected_request_id.trim() {
        return Err(invalid_upstream("GEA Interaction Request 回执 requestId 不匹配"));
    }
    if let Some(resolved_at) = receipt.resolved_at.as_deref() {
        validate_timestamp("resolvedAt", resolved_at)?;
    }
    if let Some(request) = receipt.request.as_ref() {
        validate_request(request)?;
        if request.id != receipt.request_id {
            return Err(invalid_upstream("GEA Interaction Request 回执内嵌请求不匹配"));
        }
    }
    Ok(receipt)
}

fn validate_request(request: &GeaInteractionRequest) -> Result<(), GeaError> {
    validate_identifier("request.id", &request.id)?;
    validate_identifier("request.version", &request.version)?;
    validate_non_empty("request.title", &request.title)?;
    validate_timestamp("request.updatedAt", &request.updated_at)?;
    if let Some(expires_at) = request.expires_at.as_deref() {
        validate_timestamp("request.expiresAt", expires_at)?;
    }
    if request.allowed_actions.is_empty() {
        return Err(invalid_upstream("GEA Interaction Request allowedActions 不能为空"));
    }
    let mut actions = HashSet::with_capacity(request.allowed_actions.len());
    for action in &request.allowed_actions {
        validate_identifier("request.allowedActions", action)?;
        if !actions.insert(action.as_str()) {
            return Err(invalid_upstream("GEA Interaction Request allowedActions 重复"));
        }
    }
    match (&request.kind, &request.presentation) {
        (GeaInteractionRequestKind::Question, GeaInteractionPresentation::Question { questions }) => {
            if questions.is_empty() || !actions.contains("answer") || !actions.contains("decline") {
                return Err(invalid_upstream("GEA question 必须包含问题并允许 answer 和 decline"));
            }
            for question in questions {
                validate_non_empty("presentation.question", &question.question)?;
                if question.options.is_empty() {
                    return Err(invalid_upstream("GEA question options 不能为空"));
                }
                for option in &question.options {
                    validate_non_empty("presentation.option.label", &option.label)?;
                }
            }
        }
        (GeaInteractionRequestKind::Permission, GeaInteractionPresentation::Permission { options, .. }) => {
            if options.is_empty() {
                return Err(invalid_upstream("GEA permission options 不能为空"));
            }
            for option in options {
                validate_non_empty("presentation.option.label", &option.label)?;
                validate_identifier("presentation.option.value", &option.value)?;
                if !actions.contains(option.value.as_str()) {
                    return Err(invalid_upstream("GEA permission option 不在 allowedActions 中"));
                }
            }
        }
        _ => return Err(invalid_upstream("GEA Interaction Request kind 与 presentation 不匹配")),
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str) -> Result<(), GeaError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 240 {
        return Err(invalid_upstream(format!("{field} 必须为 1 到 240 个字符")));
    }
    Ok(())
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), GeaError> {
    if value.trim().is_empty() {
        return Err(invalid_upstream(format!("{field} 不能为空")));
    }
    Ok(())
}

fn validate_timestamp(field: &str, value: &str) -> Result<(), GeaError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|_| ())
        .map_err(|_| invalid_upstream(format!("{field} 必须是带时区的 RFC 3339 时间")))
}

fn reject_sensitive_fields(value: &Value) -> Result<(), GeaError> {
    fn visit(value: &Value) -> Option<&str> {
        match value {
            Value::Array(items) => items.iter().find_map(visit),
            Value::Object(entries) => entries.iter().find_map(|(key, child)| {
                let normalized = key
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect::<String>();
                let sensitive = normalized == "authorization"
                    || normalized == "cookie"
                    || normalized == "password"
                    || normalized.ends_with("accesskey")
                    || normalized.ends_with("apikey")
                    || normalized.ends_with("secret")
                    || normalized.ends_with("token");
                sensitive.then_some(key.as_str()).or_else(|| visit(child))
            }),
            _ => None,
        }
    }

    if let Some(field) = visit(value) {
        return Err(invalid_upstream(format!(
            "GEA Interaction Request 包含敏感字段 {field}"
        )));
    }
    Ok(())
}

fn invalid_upstream(message: impl Into<String>) -> GeaError {
    GeaError::bad_gateway("GEA_INVALID_RESPONSE", message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_receipt, parse_snapshot};

    #[test]
    fn snapshot_rejects_sensitive_unknown_fields() {
        let error = parse_snapshot(&json!({
            "result": {
                "revision": "pending-r1",
                "items": [{
                    "id": "request-1",
                    "version": "v1",
                    "status": "pending",
                    "kind": "permission",
                    "title": "Confirm",
                    "allowedActions": ["proceed_once"],
                    "updatedAt": "2026-08-17T10:00:10+08:00",
                    "accessToken": "must-not-cross-the-contract",
                    "presentation": {
                        "type": "permission",
                        "title": "Confirm",
                        "description": "Execute once.",
                        "operation": "execute",
                        "options": [{ "label": "Allow", "value": "proceed_once" }]
                    }
                }]
            }
        }))
        .unwrap_err();

        assert_eq!(error.body.code, "GEA_INVALID_RESPONSE");
        assert!(error.to_string().contains("accessToken"));
    }

    #[test]
    fn receipt_rejects_a_mismatched_request_id() {
        let error = parse_receipt(
            &json!({
                "result": {
                    "receiptId": "receipt-1",
                    "requestId": "different-request",
                    "version": "v1",
                    "status": "accepted",
                    "auditId": "audit-2"
                }
            }),
            "request-1",
        )
        .unwrap_err();

        assert_eq!(error.body.code, "GEA_INVALID_RESPONSE");
        assert!(error.to_string().contains("requestId 不匹配"));
    }
}
