use std::collections::HashSet;

use aionui_api_types::{
    GeaNotification, GeaNotificationReceipt, NotificationActionCommand, NotificationStatus, NotificationTarget,
};
use serde_json::Value;

use crate::error::GeaError;

#[derive(Debug)]
pub(crate) struct GeaNotificationPage {
    pub(crate) items: Vec<GeaNotification>,
    pub(crate) total: usize,
}

pub(crate) fn parse_notification_page(value: &Value) -> Result<GeaNotificationPage, GeaError> {
    let mut result = value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA Notification 响应缺少 result"))?;
    reject_sensitive_fields(&result)?;
    let items = result
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid_upstream("GEA Notification 分页缺少 items"))?;
    for item in items {
        normalize_notification(item)?;
    }
    let total = result
        .get("total")
        .and_then(Value::as_u64)
        .and_then(|total| usize::try_from(total).ok())
        .ok_or_else(|| invalid_upstream("GEA Notification 分页 total 格式无效"))?;
    let items = serde_json::from_value::<Vec<GeaNotification>>(
        result
            .get("items")
            .cloned()
            .ok_or_else(|| invalid_upstream("GEA Notification 分页缺少 items"))?,
    )
    .map_err(|_| invalid_upstream("GEA Notification 分页格式无效"))?;
    if items.len() > total {
        return Err(invalid_upstream("GEA Notification 分页 items 数量超过 total"));
    }
    let mut ids = HashSet::with_capacity(items.len());
    for notification in &items {
        validate_notification(notification)?;
        if !ids.insert(notification.id.as_str()) {
            return Err(invalid_upstream("GEA Notification 分页包含重复 notification ID"));
        }
    }
    Ok(GeaNotificationPage { items, total })
}

pub(crate) fn parse_notification_receipt(
    value: &Value,
    expected_notification_id: &str,
) -> Result<GeaNotificationReceipt, GeaError> {
    let mut result = value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA Notification 动作响应缺少 result"))?;
    reject_sensitive_fields(&result)?;
    let result_object = result
        .as_object_mut()
        .ok_or_else(|| invalid_upstream("GEA Notification 动作 result 必须是 JSON object"))?;
    promote_alias(result_object, "receiptId", "receipt_id")?;
    promote_alias(result_object, "notificationId", "notification_id")?;
    normalize_string_version(result_object, "version")?;
    if let Some(status) = result_object.get_mut("status") {
        match status.as_str() {
            Some("already_read") => *status = Value::String("read".to_owned()),
            Some("already_dismissed") => *status = Value::String("dismissed".to_owned()),
            _ => {}
        }
    }
    if let Some(notification) = result.get_mut("notification") {
        normalize_notification(notification)?;
    }
    reject_sensitive_fields(&result)?;
    let receipt = serde_json::from_value::<GeaNotificationReceipt>(result)
        .map_err(|_| invalid_upstream("GEA Notification 回执格式无效"))?;
    validate_identifier("receiptId", &receipt.receipt_id)?;
    validate_identifier("notificationId", &receipt.notification_id)?;
    validate_identifier("version", &receipt.version)?;
    if receipt.notification_id != expected_notification_id.trim() {
        return Err(invalid_upstream("GEA Notification 回执 notificationId 不匹配"));
    }
    if let Some(notification) = receipt.notification.as_ref() {
        validate_notification(notification)?;
        if notification.id != receipt.notification_id {
            return Err(invalid_upstream("GEA Notification 回执内嵌通知不匹配"));
        }
    }
    Ok(receipt)
}

pub(crate) fn validate_notification_action(
    notification_id: &str,
    command: &NotificationActionCommand,
) -> Result<(), GeaError> {
    for (field, value) in [
        ("notificationId", notification_id),
        ("expectedVersion", command.expected_version.as_str()),
        ("idempotencyKey", command.idempotency_key.as_str()),
    ] {
        validate_identifier(field, value)
            .map_err(|_| GeaError::invalid_request(format!("{field} 必须为 1 到 240 个字符")))?;
    }
    Ok(())
}

fn validate_notification(notification: &GeaNotification) -> Result<(), GeaError> {
    validate_identifier("notification.id", &notification.id)?;
    validate_identifier("notification.version", &notification.version)?;
    validate_non_empty("notification.title", &notification.title)?;
    validate_optional_text("notification.summary", notification.summary.as_deref(), 10_000)?;
    validate_optional_text("notification.body", notification.body.as_deref(), 100_000)?;
    validate_identifier("notification.source", &notification.source)?;
    validate_timestamp("notification.createdAt", &notification.created_at)?;
    if let Some(expires_at) = notification.expires_at.as_deref() {
        validate_timestamp("notification.expiresAt", expires_at)?;
    }
    validate_target(&notification.target)?;
    if notification.status == NotificationStatus::Dismissed && !notification.dismissible {
        return Err(invalid_upstream("不可关闭通知不能处于 dismissed 状态"));
    }
    Ok(())
}

fn validate_target(target: &NotificationTarget) -> Result<(), GeaError> {
    match target {
        NotificationTarget::Notification => Ok(()),
        NotificationTarget::Conversation { conversation_id } => validate_identifier("conversationId", conversation_id),
        NotificationTarget::Message {
            conversation_id,
            message_id,
        } => {
            validate_identifier("conversationId", conversation_id)?;
            validate_identifier("messageId", message_id)
        }
        NotificationTarget::Team { team_id } => validate_identifier("teamId", team_id),
        NotificationTarget::Slot { team_id, slot_id } => {
            validate_identifier("teamId", team_id)?;
            validate_identifier("slotId", slot_id)
        }
        NotificationTarget::InteractionRequest {
            request_id,
            conversation_id,
            team_id,
            slot_id,
        } => {
            validate_identifier("requestId", request_id)?;
            for (field, value) in [
                ("conversationId", conversation_id.as_deref()),
                ("teamId", team_id.as_deref()),
                ("slotId", slot_id.as_deref()),
            ] {
                if let Some(value) = value {
                    validate_identifier(field, value)?;
                }
            }
            Ok(())
        }
    }
}

fn normalize_notification(value: &mut Value) -> Result<(), GeaError> {
    let notification = value
        .as_object_mut()
        .ok_or_else(|| invalid_upstream("GEA Notification 条目必须是 JSON object"))?;
    promote_alias(notification, "notificationId", "id")?;
    promote_alias(notification, "status", "state")?;
    promote_alias(notification, "interactionRequestId", "interaction_request_id")?;
    promote_alias(notification, "createdAt", "created_at")?;
    promote_alias(notification, "expiresAt", "expires_at")?;
    normalize_string_version(notification, "version")?;
    normalize_kind(notification);
    normalize_severity(notification);
    if let Some(source) = notification.get_mut("source") {
        normalize_source(source);
    }
    let interaction_request_id = notification
        .get("interactionRequestId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if let Some(request_id) = interaction_request_id {
        notification.insert(
            "target".to_owned(),
            serde_json::json!({"type": "interaction_request", "requestId": request_id}),
        );
    } else {
        let target = notification
            .entry("target")
            .or_insert_with(|| serde_json::json!({"type": "notification"}));
        normalize_target(target);
    }
    Ok(())
}

fn normalize_target(value: &mut Value) {
    let Some(target) = value.as_object_mut() else {
        *value = serde_json::json!({"type": "notification"});
        return;
    };
    let target_type = target.get("type").and_then(Value::as_str).map(str::to_owned);
    let shorthand_value = target.get("value").and_then(Value::as_str).map(str::to_owned);
    if let (Some(target_type), Some(shorthand_value)) = (target_type.as_deref(), shorthand_value) {
        let field = match target_type {
            "conversation" => Some("conversationId"),
            "team" => Some("teamId"),
            "interaction_request" => Some("requestId"),
            _ => None,
        };
        if let Some(field) = field {
            target.entry(field.to_owned()).or_insert(Value::String(shorthand_value));
        }
    }
    if target_type.as_deref() == Some("aggregate") {
        *value = serde_json::json!({"type": "notification"});
        return;
    }
    let Some(target) = value.as_object() else {
        return;
    };
    let valid_identifier = |field: &str| {
        target
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty() && value.chars().count() <= 240)
    };
    let valid = match target.get("type").and_then(Value::as_str) {
        Some("notification") => true,
        Some("conversation") => valid_identifier("conversationId"),
        Some("message") => valid_identifier("conversationId") && valid_identifier("messageId"),
        Some("team") => valid_identifier("teamId"),
        Some("slot") => valid_identifier("teamId") && valid_identifier("slotId"),
        Some("interaction_request") => valid_identifier("requestId"),
        _ => false,
    };
    if !valid {
        *value = serde_json::json!({"type": "notification"});
    }
}

fn promote_alias(object: &mut serde_json::Map<String, Value>, canonical: &str, alias: &str) -> Result<(), GeaError> {
    if let (Some(canonical_value), Some(alias_value)) = (object.get(canonical), object.get(alias))
        && canonical_value != alias_value
    {
        return Err(invalid_upstream(format!(
            "GEA Notification {canonical} 与 {alias} 不匹配"
        )));
    }
    if !object.contains_key(canonical)
        && let Some(value) = object.remove(alias)
    {
        object.insert(canonical.to_owned(), value);
    }
    Ok(())
}

fn normalize_string_version(object: &mut serde_json::Map<String, Value>, field: &str) -> Result<(), GeaError> {
    let Some(version) = object.get_mut(field) else {
        return Ok(());
    };
    match version {
        Value::String(_) => Ok(()),
        Value::Number(number) => {
            *version = Value::String(number.to_string());
            Ok(())
        }
        _ => Err(invalid_upstream(format!("GEA Notification {field} 格式无效"))),
    }
}

fn normalize_kind(object: &mut serde_json::Map<String, Value>) {
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("message") => "message",
        Some("reminder") => "reminder",
        Some("todo" | "approval" | "action_required") => "action_required",
        Some("system") => "system",
        Some("notice" | "alert" | "event") | None => "event",
        Some(_) => "event",
    };
    object.insert("kind".to_owned(), Value::String(kind.to_owned()));
}

fn normalize_severity(object: &mut serde_json::Map<String, Value>) {
    let severity = match object.get("severity").and_then(Value::as_str) {
        Some("critical") => "critical",
        Some("medium" | "high" | "warning") => "warning",
        Some("success") => "success",
        Some("info" | "low") | None => "info",
        Some(_) => "info",
    };
    object.insert("severity".to_owned(), Value::String(severity.to_owned()));
}

fn normalize_source(value: &mut Value) {
    if let Some(source) = value.as_str() {
        let source = source.trim();
        if source.is_empty() {
            *value = Value::String("gea".to_owned());
        }
        return;
    }
    let display = value
        .as_object()
        .and_then(|source| {
            ["label", "ref", "type"]
                .into_iter()
                .filter_map(|field| source.get(field).and_then(Value::as_str))
                .map(str::trim)
                .find(|value| !value.is_empty())
        })
        .unwrap_or("gea")
        .to_owned();
    *value = Value::String(display);
}

fn validate_identifier(field: &str, value: &str) -> Result<(), GeaError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 240 {
        return Err(invalid_upstream(format!("{field} 必须为 1 到 240 个字符")));
    }
    Ok(())
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), GeaError> {
    if value.trim().is_empty() || value.chars().count() > 4000 {
        return Err(invalid_upstream(format!("{field} 必须为 1 到 4000 个字符")));
    }
    Ok(())
}

fn validate_optional_text(field: &str, value: Option<&str>, maximum: usize) -> Result<(), GeaError> {
    if value.is_some_and(|value| value.chars().count() > maximum) {
        return Err(invalid_upstream(format!("{field} 不能超过 {maximum} 个字符")));
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
        return Err(invalid_upstream(format!("GEA Notification 包含敏感字段 {field}")));
    }
    Ok(())
}

fn invalid_upstream(message: impl Into<String>) -> GeaError {
    GeaError::bad_gateway("GEA_INVALID_RESPONSE", message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_notification_page, parse_notification_receipt};

    #[test]
    fn parses_current_gea_notification_page_and_normalizes_the_client_contract() {
        let page = parse_notification_page(&json!({
            "result": {
                "items": [{
                    "id": "notification-1",
                    "version": 1,
                    "state": "unread",
                    "kind": "workflow_alert",
                    "severity": "minor",
                    "title": "Budget threshold reached",
                    "dismissible": true,
                    "source": {"type": "business_system", "ref": "budget-1", "label": "budget.threshold"},
                    "target": {"type": "aggregate", "value": "budget-1"},
                    "interaction_request_id": "interaction-1",
                    "created_at": "2026-08-22T10:00:00+08:00"
                }],
                "unread_count": 1,
                "total": 1
            }
        }))
        .unwrap();
        let notification = &page.items[0];
        assert_eq!(page.total, 1);
        assert_eq!(notification.id, "notification-1");
        assert_eq!(notification.version, "1");
        assert_eq!(notification.source, "budget.threshold");
        assert!(matches!(notification.kind, aionui_api_types::NotificationKind::Event));
        assert!(matches!(
            notification.severity,
            aionui_api_types::NotificationSeverity::Info
        ));
        assert!(matches!(
            notification.target,
            aionui_api_types::NotificationTarget::InteractionRequest { ref request_id, .. }
                if request_id == "interaction-1"
        ));
    }

    #[test]
    fn parses_a_multi_item_current_gea_notification_page() {
        let page = parse_notification_page(&json!({
            "result": {
                "items": [{
                    "id": "notification-1", "version": 1, "state": "unread",
                    "kind": "workflow_event", "severity": "warning", "title": "Forecast needs review",
                    "summary": "September forecast", "dismissible": true,
                    "source": {"type": "business_system", "ref": "forecast-1", "label": "gea.workflow"},
                    "target": {"type": "aggregate", "value": "forecast-1"},
                    "created_at": "2026-08-22T08:00:00Z"
                }, {
                    "id": "notification-expired", "version": 1, "state": "unread",
                    "kind": "reminder", "severity": "info", "title": "Expired reminder",
                    "dismissible": true,
                    "source": {"type": "business_system", "ref": "reminder-1", "label": "gea.workflow"},
                    "target": {"type": "aggregate", "value": "reminder-1"},
                    "created_at": "2020-01-01T00:00:00Z", "expires_at": "2020-01-02T00:00:00Z"
                }],
                "unread_count": 2,
                "total": 2
            }
        }))
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 2);
    }

    #[test]
    fn rejects_sensitive_fields_in_current_gea_notification_pages() {
        let error = parse_notification_page(&json!({
            "result": {"items": [{
                "id": "notification-2", "version": 1, "state": "unread",
                "kind": "event", "severity": "info", "title": "Unsafe", "dismissible": true,
                "source": {"type": "gea"}, "target": {"type": "aggregate", "value": "aggregate-1"},
                "created_at": "2026-08-22T10:00:00+08:00", "accessToken": "secret"
            }], "unread_count": 1, "total": 1}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("accessToken"));
    }

    #[test]
    fn rejects_mismatched_notification_receipt() {
        let error = parse_notification_receipt(
            &json!({"result": {
                "receipt_id": "receipt-1", "notification_id": "other", "version": 2, "status": "read"
            }}),
            "notification-1",
        )
        .unwrap_err();
        assert!(error.to_string().contains("notificationId 不匹配"));
    }

    #[test]
    fn parses_current_gea_notification_receipt() {
        let receipt = parse_notification_receipt(
            &json!({"result": {
                "receipt_id": "receipt-1",
                "notification_id": "notification-1",
                "version": 2,
                "status": "already_read"
            }}),
            "notification-1",
        )
        .unwrap();
        assert_eq!(receipt.receipt_id, "receipt-1");
        assert_eq!(receipt.version, "2");
        assert!(matches!(receipt.status, aionui_api_types::NotificationStatus::Read));
    }

    #[test]
    fn defaults_missing_gea_event_source_and_target_to_safe_client_values() {
        let page = parse_notification_page(&json!({
            "result": {"items": [{
                "id": "notification-orphan", "version": 1, "state": "unread",
                "kind": "event", "severity": "info", "title": "Orphaned event", "dismissible": true,
                "source": null, "target": null, "created_at": "2026-08-22T10:00:00+08:00"
            }], "unread_count": 1, "total": 1}
        }))
        .unwrap();
        assert_eq!(page.items[0].source, "gea");
        assert!(matches!(
            page.items[0].target,
            aionui_api_types::NotificationTarget::Notification
        ));
    }

    #[test]
    fn maps_the_frozen_gea_interaction_request_target_value() {
        let page = parse_notification_page(&json!({
            "result": {"items": [{
                "id": "notification-interaction", "version": 1, "state": "unread",
                "kind": "approval", "severity": "high", "title": "Approval", "dismissible": false,
                "source": {"type": "business_system", "ref": "approval-1"},
                "target": {"type": "interaction_request", "value": "request-1"},
                "created_at": "2026-08-22T10:00:00+08:00"
            }], "unread_count": 1, "total": 1}
        }))
        .unwrap();
        assert!(matches!(
            page.items[0].target,
            aionui_api_types::NotificationTarget::InteractionRequest { ref request_id, .. }
                if request_id == "request-1"
        ));
    }

    #[test]
    fn maps_the_frozen_gea_kind_and_severity_values_to_client_semantics() {
        use aionui_api_types::{NotificationKind, NotificationSeverity};

        for (kind, expected_kind, severity, expected_severity) in [
            (
                "todo",
                NotificationKind::ActionRequired,
                "low",
                NotificationSeverity::Info,
            ),
            (
                "approval",
                NotificationKind::ActionRequired,
                "medium",
                NotificationSeverity::Warning,
            ),
            ("notice", NotificationKind::Event, "high", NotificationSeverity::Warning),
            (
                "reminder",
                NotificationKind::Reminder,
                "critical",
                NotificationSeverity::Critical,
            ),
            ("alert", NotificationKind::Event, "info", NotificationSeverity::Info),
            ("message", NotificationKind::Message, "info", NotificationSeverity::Info),
        ] {
            let page = parse_notification_page(&json!({
                "result": {"items": [{
                    "id": format!("notification-{kind}"), "version": 1, "state": "unread",
                    "kind": kind, "severity": severity, "title": "Mapped", "dismissible": true,
                    "source": {"type": "business_system", "ref": "aggregate-1"},
                    "target": {"type": "aggregate", "value": "aggregate-1"},
                    "created_at": "2026-08-22T10:00:00+08:00"
                }], "unread_count": 1, "total": 1}
            }))
            .unwrap();
            assert_eq!(page.items[0].kind, expected_kind);
            assert_eq!(page.items[0].severity, expected_severity);
        }
    }

    #[test]
    fn downgrades_unknown_or_incomplete_targets_to_the_safe_notification_target() {
        for target in [
            json!({"type": "javascript", "url": "javascript:alert(1)"}),
            json!({"type": "message"}),
        ] {
            let page = parse_notification_page(&json!({
                "result": {"items": [{
                    "id": "notification-1", "version": 1, "state": "unread",
                    "kind": "event", "severity": "info", "title": "Safe fallback", "dismissible": true,
                    "source": "gea", "target": target, "created_at": "2026-08-22T10:00:00+08:00"
                }], "unread_count": 1, "total": 1}
            }))
            .unwrap();
            assert!(matches!(
                page.items[0].target,
                aionui_api_types::NotificationTarget::Notification
            ));
        }
    }
}
