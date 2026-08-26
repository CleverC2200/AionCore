use std::collections::HashSet;

use aionui_api_types::{
    GeaInteractionPresentation, GeaInteractionRequest, GeaInteractionRequestActionCommand, GeaInteractionRequestKind,
    GeaInteractionRequestReceipt, GeaInteractionRequestSnapshot, GeaInteractionRequestStatus,
};
use chrono::{NaiveDateTime, SecondsFormat, TimeZone};
use chrono_tz::Asia::Shanghai;
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
    let mut result = value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA Interaction Request 响应缺少 result"))?;
    let items = result
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid_upstream("GEA Interaction Request 快照缺少 items"))?;
    for request in items {
        normalize_request(request)?;
    }
    reject_sensitive_fields(&result)?;
    let snapshot = serde_json::from_value::<GeaInteractionRequestSnapshot>(result)
        .map_err(|_| invalid_upstream("GEA Interaction Request 快照格式无效"))?;
    validate_identifier("revision", &snapshot.revision)?;
    let mut ids = HashSet::with_capacity(snapshot.items.len());
    for request in &snapshot.items {
        validate_request(request)?;
        if request.status == GeaInteractionRequestStatus::Cancelled {
            return Err(invalid_upstream("GEA 完整快照不应包含 cancelled 请求"));
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
    let mut result = value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA Interaction Request 动作响应缺少 result"))?;
    if let Some(receipt) = result.as_object_mut() {
        normalize_timestamp_field(receipt, "resolvedAt", "resolvedAt")?;
    }
    if let Some(request) = result.get_mut("request") {
        normalize_request(request)?;
    }
    reject_sensitive_fields(&result)?;
    let receipt = serde_json::from_value::<GeaInteractionRequestReceipt>(result)
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
    if !request.updated_at.trim().is_empty() {
        validate_timestamp("request.updatedAt", &request.updated_at)?;
    }
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
            if !questions.is_empty() && (!actions.contains("answer") || !actions.contains("decline")) {
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

fn normalize_request(value: &mut Value) -> Result<(), GeaError> {
    let request = value
        .as_object_mut()
        .ok_or_else(|| invalid_upstream("GEA Interaction Request 条目必须是 JSON object"))?;
    let external_id = request.get("requestId").and_then(Value::as_str);
    let legacy_id = request.get("id").and_then(Value::as_str);
    if external_id.is_some() && legacy_id.is_some() && external_id != legacy_id {
        return Err(invalid_upstream("GEA Interaction Request requestId 与 id 不匹配"));
    }
    if external_id.is_none()
        && let Some(id) = request.remove("id")
    {
        request.insert("requestId".to_owned(), id);
    }
    normalize_timestamp_field(request, "updatedAt", "request.updatedAt")?;
    normalize_timestamp_field(request, "expiresAt", "request.expiresAt")?;
    if request.get("presentation").is_none_or(Value::is_null) {
        let kind = request
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_upstream("GEA Interaction Request 缺少 kind"))?;
        request.insert("presentation".to_owned(), serde_json::json!({ "type": kind }));
    }
    if let Some(Value::String(serialized)) = request.get("presentation") {
        let presentation = serde_json::from_str::<Value>(serialized)
            .map_err(|_| invalid_upstream("GEA Interaction Request presentation 不是有效 JSON"))?;
        if !presentation.is_object() {
            return Err(invalid_upstream(
                "GEA Interaction Request presentation 必须是 JSON object",
            ));
        }
        request.insert("presentation".to_owned(), presentation);
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

fn normalize_timestamp_field(
    object: &mut serde_json::Map<String, Value>,
    key: &str,
    field: &str,
) -> Result<(), GeaError> {
    let Some(Value::String(value)) = object.get_mut(key) else {
        return Ok(());
    };
    *value = normalize_timestamp(field, value)?;
    Ok(())
}

fn normalize_timestamp(field: &str, value: &str) -> Result<String, GeaError> {
    if value.trim().is_empty() || chrono::DateTime::parse_from_rfc3339(value).is_ok() {
        return Ok(value.to_owned());
    }

    let local = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .map_err(|_| invalid_upstream(format!("{field} 必须是带时区的 RFC 3339 或 yyyy-MM-dd HH:mm:ss 时间")))?;
    let timestamp = Shanghai
        .from_local_datetime(&local)
        .single()
        .ok_or_else(|| invalid_upstream(format!("{field} 无法按 GEA 服务时区 Asia/Shanghai 唯一确定")))?;
    Ok(timestamp.to_rfc3339_opts(SecondsFormat::Secs, false))
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
    fn snapshot_normalizes_current_v1_1_request_shape() {
        let snapshot = parse_snapshot(&json!({
            "result": {
                "revision": "gea-pending-3",
                "items": [{
                    "requestId": "request-001",
                    "version": "v1",
                    "kind": "question",
                    "status": "pending",
                    "title": "请确认报表数据范围",
                    "summary": "用于生成本月销售分析",
                    "sourceLabel": "sales-analysis",
                    "allowedActions": ["answer", "decline"],
                    "presentation": "{\"type\":\"question\"}",
                    "expiresAt": "2026-08-21T12:00:00+08:00"
                }]
            }
        }))
        .unwrap();

        assert_eq!(snapshot.items[0].id, "request-001");
        assert!(snapshot.items[0].updated_at.is_empty());
        assert!(matches!(
            snapshot.items[0].presentation,
            aionui_api_types::GeaInteractionPresentation::Question { ref questions } if questions.is_empty()
        ));
    }

    #[test]
    fn snapshot_normalizes_gea_legacy_timestamps_to_rfc3339() {
        let snapshot = parse_snapshot(&json!({
            "result": {
                "revision": "gea-pending-legacy-time",
                "items": [{
                    "requestId": "request-legacy-time",
                    "version": "v1",
                    "kind": "question",
                    "status": "pending",
                    "title": "请确认预测范围",
                    "allowedActions": ["answer", "decline"],
                    "presentation": {"type": "question"},
                    "updatedAt": "2026-08-26 20:10:32",
                    "expiresAt": "2026-08-26 21:10:32"
                }]
            }
        }))
        .unwrap();

        assert_eq!(snapshot.items[0].updated_at, "2026-08-26T20:10:32+08:00");
        assert_eq!(
            snapshot.items[0].expires_at.as_deref(),
            Some("2026-08-26T21:10:32+08:00")
        );
    }

    #[test]
    fn snapshot_preserves_rfc3339_timestamps() {
        let snapshot = parse_snapshot(&json!({
            "result": {
                "revision": "gea-pending-rfc3339-time",
                "items": [{
                    "requestId": "request-rfc3339-time",
                    "version": "v1",
                    "kind": "question",
                    "status": "pending",
                    "title": "请确认预测范围",
                    "allowedActions": ["answer", "decline"],
                    "presentation": {"type": "question"},
                    "updatedAt": "2026-08-26T20:10:32+08:00",
                    "expiresAt": "2026-08-26T21:10:32+08:00"
                }]
            }
        }))
        .unwrap();

        assert_eq!(snapshot.items[0].updated_at, "2026-08-26T20:10:32+08:00");
        assert_eq!(
            snapshot.items[0].expires_at.as_deref(),
            Some("2026-08-26T21:10:32+08:00")
        );
    }

    #[test]
    fn snapshot_rejects_invalid_legacy_timestamp() {
        let error = parse_snapshot(&json!({
            "result": {
                "revision": "gea-pending-invalid-time",
                "items": [{
                    "requestId": "request-invalid-time",
                    "version": "v1",
                    "kind": "question",
                    "status": "pending",
                    "title": "请确认预测范围",
                    "allowedActions": ["answer", "decline"],
                    "presentation": {"type": "question"},
                    "updatedAt": "2026-02-30 20:10:32"
                }]
            }
        }))
        .unwrap_err();

        assert_eq!(error.body.code, "GEA_INVALID_RESPONSE");
        assert!(error.to_string().contains("yyyy-MM-dd HH:mm:ss"));
    }

    #[test]
    fn snapshot_accepts_the_complete_non_cancelled_v1_1_status_set() {
        let snapshot = parse_snapshot(&json!({
            "result": {
                "revision": "gea-all-4",
                "items": [
                    {
                        "requestId": "request-processing",
                        "version": "v2",
                        "kind": "question",
                        "status": "processing",
                        "title": "正在提交",
                        "allowedActions": ["answer", "decline"],
                        "presentation": "{\"type\":\"question\"}"
                    },
                    {
                        "requestId": "request-resolved",
                        "version": "v3",
                        "kind": "permission",
                        "status": "resolved",
                        "title": "已完成",
                        "allowedActions": ["proceed_once"],
                        "presentation": "{\"type\":\"permission\"}"
                    },
                    {
                        "requestId": "request-expired",
                        "version": "v2",
                        "kind": "permission",
                        "status": "expired",
                        "title": "已过期",
                        "allowedActions": ["proceed_once"]
                    }
                ]
            }
        }))
        .unwrap();

        assert_eq!(snapshot.items.len(), 3);
        assert_eq!(
            snapshot.items[0].status,
            aionui_api_types::GeaInteractionRequestStatus::Processing
        );
    }

    #[test]
    fn snapshot_accepts_verification_required_as_active() {
        let snapshot = parse_snapshot(&json!({
            "result": {
                "revision": "active-r2",
                "items": [{
                    "requestId": "request-1",
                    "version": "v2",
                    "status": "verification_required",
                    "kind": "permission",
                    "title": "Verify",
                    "allowedActions": ["verify_succeeded", "verify_failed"],
                    "presentation": {
                        "type": "permission",
                        "title": "Verify",
                        "description": "Verify the external result.",
                        "operation": "verify",
                        "options": [
                            { "label": "Succeeded", "value": "verify_succeeded" },
                            { "label": "Failed", "value": "verify_failed" }
                        ]
                    }
                }]
            }
        }))
        .unwrap();

        assert_eq!(
            snapshot.items[0].status,
            aionui_api_types::GeaInteractionRequestStatus::VerificationRequired
        );
    }

    #[test]
    fn snapshot_rejects_sensitive_fields_inside_string_presentation() {
        let error = parse_snapshot(&json!({
            "result": {
                "revision": "pending-r1",
                "items": [{
                    "requestId": "request-1",
                    "version": "v1",
                    "status": "pending",
                    "kind": "permission",
                    "title": "Confirm",
                    "allowedActions": ["proceed_once"],
                    "presentation": "{\"type\":\"permission\",\"title\":\"Confirm\",\"description\":\"Execute once.\",\"operation\":\"execute\",\"accessToken\":\"secret\",\"options\":[{\"label\":\"Allow\",\"value\":\"proceed_once\"}]}"
                }]
            }
        }))
        .unwrap_err();

        assert!(error.to_string().contains("accessToken"));
    }

    #[test]
    fn snapshot_rejects_mismatched_request_identifiers() {
        let error = parse_snapshot(&json!({
            "result": {
                "revision": "pending-r1",
                "items": [{
                    "requestId": "request-1",
                    "id": "request-2",
                    "version": "v1",
                    "status": "pending",
                    "kind": "permission",
                    "title": "Confirm",
                    "allowedActions": ["proceed_once"],
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

        assert!(error.to_string().contains("requestId 与 id 不匹配"));
    }

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

    #[test]
    fn receipt_accepts_v1_1_processing_and_failed_results() {
        for status in ["processing", "failed"] {
            let receipt = parse_receipt(
                &json!({
                    "result": {
                        "receiptId": format!("receipt-{status}"),
                        "requestId": "request-1",
                        "version": "v2",
                        "status": status
                    }
                }),
                "request-1",
            )
            .unwrap();

            assert_eq!(
                receipt.status,
                if status == "processing" {
                    aionui_api_types::GeaInteractionRequestReceiptStatus::Processing
                } else {
                    aionui_api_types::GeaInteractionRequestReceiptStatus::Failed
                }
            );
        }
    }

    #[test]
    fn receipt_normalizes_gea_legacy_resolved_at_to_rfc3339() {
        let receipt = parse_receipt(
            &json!({
                "result": {
                    "receiptId": "receipt-legacy-time",
                    "requestId": "request-1",
                    "version": "v2",
                    "status": "accepted",
                    "resolvedAt": "2026-08-26 20:15:32"
                }
            }),
            "request-1",
        )
        .unwrap();

        assert_eq!(receipt.resolved_at.as_deref(), Some("2026-08-26T20:15:32+08:00"));
    }
}
