use std::collections::HashSet;

use aionui_api_types::{
    GeaNotification, GeaNotificationReceipt, GeaNotificationSnapshot, NotificationActionCommand, NotificationStatus,
    NotificationTarget,
};
use serde_json::Value;

use crate::error::GeaError;

pub(crate) fn parse_notification_snapshot(value: &Value) -> Result<GeaNotificationSnapshot, GeaError> {
    let mut result = value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA Notification 响应缺少 result"))?;
    let items = result
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid_upstream("GEA Notification 快照缺少 items"))?;
    for item in items {
        normalize_notification(item)?;
    }
    reject_sensitive_fields(&result)?;
    let snapshot = serde_json::from_value::<GeaNotificationSnapshot>(result)
        .map_err(|_| invalid_upstream("GEA Notification 快照格式无效"))?;
    validate_identifier("revision", &snapshot.revision)?;
    if let Some(next_cursor) = snapshot.next_cursor.as_deref() {
        validate_identifier("nextCursor", next_cursor)?;
    }
    let mut ids = HashSet::with_capacity(snapshot.items.len());
    for notification in &snapshot.items {
        validate_notification(notification)?;
        if !ids.insert(notification.id.as_str()) {
            return Err(invalid_upstream("GEA Notification 快照包含重复 notification ID"));
        }
    }
    Ok(snapshot)
}

pub(crate) fn parse_notification_receipt(
    value: &Value,
    expected_notification_id: &str,
) -> Result<GeaNotificationReceipt, GeaError> {
    let mut result = value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA Notification 动作响应缺少 result"))?;
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
    let external_id = notification.get("notificationId").and_then(Value::as_str);
    let legacy_id = notification.get("id").and_then(Value::as_str);
    if external_id.is_some() && legacy_id.is_some() && external_id != legacy_id {
        return Err(invalid_upstream("GEA Notification notificationId 与 id 不匹配"));
    }
    if external_id.is_none()
        && let Some(id) = notification.remove("id")
    {
        notification.insert("notificationId".to_owned(), id);
    }
    let target = notification
        .entry("target")
        .or_insert_with(|| serde_json::json!({"type": "notification"}));
    normalize_target(target);
    Ok(())
}

fn normalize_target(value: &mut Value) {
    let Some(target) = value.as_object() else {
        *value = serde_json::json!({"type": "notification"});
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

    use super::{parse_notification_receipt, parse_notification_snapshot};

    #[test]
    fn parses_notification_snapshot_and_rejects_sensitive_fields() {
        let snapshot = parse_notification_snapshot(&json!({
            "result": {
                "revision": "notifications-r1",
                "items": [{
                    "notificationId": "notification-1",
                    "version": "v1",
                    "status": "unread",
                    "kind": "event",
                    "severity": "warning",
                    "title": "Budget threshold reached",
                    "dismissible": true,
                    "source": "gea.workflow",
                    "target": {"type": "conversation", "conversationId": "conversation-1"},
                    "createdAt": "2026-08-22T10:00:00+08:00"
                }]
            }
        }))
        .unwrap();
        assert_eq!(snapshot.items[0].id, "notification-1");

        let error = parse_notification_snapshot(&json!({
            "result": {"revision": "r2", "items": [{
                "notificationId": "notification-2", "version": "v1", "status": "unread",
                "kind": "event", "severity": "info", "title": "Unsafe", "dismissible": true,
                "source": "gea", "target": {"type": "notification"},
                "createdAt": "2026-08-22T10:00:00+08:00", "accessToken": "secret"
            }]}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("accessToken"));
    }

    #[test]
    fn rejects_mismatched_notification_receipt() {
        let error = parse_notification_receipt(
            &json!({"result": {
                "receiptId": "receipt-1", "notificationId": "other", "version": "v2", "status": "read"
            }}),
            "notification-1",
        )
        .unwrap_err();
        assert!(error.to_string().contains("notificationId 不匹配"));
    }

    #[test]
    fn downgrades_unknown_or_incomplete_targets_to_the_safe_notification_target() {
        for target in [
            json!({"type": "javascript", "url": "javascript:alert(1)"}),
            json!({"type": "message"}),
        ] {
            let snapshot = parse_notification_snapshot(&json!({
                "result": {"revision": "r1", "items": [{
                    "notificationId": "notification-1", "version": "v1", "status": "unread",
                    "kind": "event", "severity": "info", "title": "Safe fallback", "dismissible": true,
                    "source": "gea", "target": target, "createdAt": "2026-08-22T10:00:00+08:00"
                }]}
            }))
            .unwrap();
            assert!(matches!(
                snapshot.items[0].target,
                aionui_api_types::NotificationTarget::Notification
            ));
        }
    }
}
