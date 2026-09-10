use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::error::GeaError;

const MCP_ENDPOINT: &str = "/ai/gateway/mcp/proxy/mcp";
const PREFERRED_PROTOCOL_VERSION: &str = "2025-11-25";
const COMPATIBLE_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2024-11-05"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpSessionHeaders {
    session_id: String,
    protocol_version: String,
}

#[derive(Debug, Default)]
pub(super) struct McpTransportSession {
    headers: Mutex<Option<McpSessionHeaders>>,
}

pub(super) struct McpTransportClient {
    client: reqwest::Client,
    endpoint: String,
    next_request_id: AtomicU64,
}

impl McpTransportClient {
    pub(super) fn new(client: reqwest::Client, base_url: &str) -> Self {
        Self {
            client,
            endpoint: format!("{base_url}{MCP_ENDPOINT}"),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub(super) async fn request(
        &self,
        auth_headers: &HeaderMap,
        session: &McpTransportSession,
        method: &str,
        params: Value,
    ) -> Result<Value, GeaError> {
        let mut headers = self.ensure_initialized(auth_headers, session).await?;
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let response = self.post(auth_headers, Some(&headers), &body).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            self.clear_if_current(session, &headers).await;
            headers = self.ensure_initialized(auth_headers, session).await?;
            let response = self.post(auth_headers, Some(&headers), &body).await?;
            return parse_json_rpc_response(response).await;
        }
        parse_json_rpc_response(response).await
    }

    pub(super) async fn close(&self, auth_headers: &HeaderMap, session: &McpTransportSession) -> Result<(), GeaError> {
        let Some(session_headers) = session.headers.lock().await.take() else {
            return Ok(());
        };
        let mut headers = auth_headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));
        headers.insert(
            "mcp-session-id",
            HeaderValue::from_str(&session_headers.session_id)
                .map_err(|_| GeaError::server_error("GEA_MCP_SESSION_INVALID", "GEA MCP Session Header 无效"))?,
        );
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_str(&session_headers.protocol_version)
                .map_err(|_| GeaError::server_error("GEA_MCP_PROTOCOL_INVALID", "GEA MCP Protocol Header 无效"))?,
        );
        let response = self
            .client
            .delete(&self.endpoint)
            .headers(headers)
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法关闭 GEA MCP Session"))?;
        if matches!(
            response.status(),
            reqwest::StatusCode::NO_CONTENT | reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
        ) {
            Ok(())
        } else {
            Err(http_error(response.status()))
        }
    }

    async fn ensure_initialized(
        &self,
        auth_headers: &HeaderMap,
        session: &McpTransportSession,
    ) -> Result<McpSessionHeaders, GeaError> {
        let mut stored = session.headers.lock().await;
        if let Some(headers) = stored.as_ref() {
            return Ok(headers.clone());
        }

        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .post(
                auth_headers,
                None,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": PREFERRED_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": {
                            "name": "aion-core",
                            "version": env!("CARGO_PKG_VERSION"),
                        }
                    }
                }),
            )
            .await?;
        let response_headers = response.headers().clone();
        let value = parse_json_rpc_response(response).await?;
        let protocol_version = response_headers
            .get("mcp-protocol-version")
            .and_then(|value| value.to_str().ok())
            .or_else(|| value.pointer("/protocolVersion").and_then(Value::as_str))
            .filter(|version| COMPATIBLE_PROTOCOL_VERSIONS.contains(version))
            .ok_or_else(|| GeaError::bad_gateway("GEA_MCP_PROTOCOL_UNSUPPORTED", "GEA MCP 返回了不支持的协议版本"))?
            .to_owned();
        let session_id = response_headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                GeaError::bad_gateway(
                    "GEA_MCP_SESSION_HEADER_MISSING",
                    "GEA MCP 初始化响应缺少 Session Header",
                )
            })?
            .to_owned();
        let headers = McpSessionHeaders {
            session_id,
            protocol_version,
        };

        let initialized = self
            .post(
                auth_headers,
                Some(&headers),
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                }),
            )
            .await?;
        if initialized.status() != reqwest::StatusCode::ACCEPTED {
            return Err(http_error(initialized.status()));
        }
        *stored = Some(headers.clone());
        tracing::info!(
            protocol_version = headers.protocol_version,
            "GEA MCP transport session initialized"
        );
        Ok(headers)
    }

    async fn clear_if_current(&self, session: &McpTransportSession, current: &McpSessionHeaders) {
        let mut stored = session.headers.lock().await;
        if stored.as_ref() == Some(current) {
            *stored = None;
            tracing::warn!("GEA MCP transport session expired; reinitializing");
        }
    }

    async fn post(
        &self,
        auth_headers: &HeaderMap,
        session_headers: Option<&McpSessionHeaders>,
        body: &Value,
    ) -> Result<reqwest::Response, GeaError> {
        let mut headers = auth_headers.clone();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        if let Some(session_headers) = session_headers {
            headers.insert(
                "mcp-session-id",
                HeaderValue::from_str(&session_headers.session_id)
                    .map_err(|_| GeaError::server_error("GEA_MCP_SESSION_INVALID", "GEA MCP Session Header 无效"))?,
            );
            headers.insert(
                "mcp-protocol-version",
                HeaderValue::from_str(&session_headers.protocol_version)
                    .map_err(|_| GeaError::server_error("GEA_MCP_PROTOCOL_INVALID", "GEA MCP Protocol Header 无效"))?,
            );
        }
        self.client
            .post(&self.endpoint)
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA MCP 服务"))
    }
}

async fn parse_json_rpc_response(response: reqwest::Response) -> Result<Value, GeaError> {
    let status = response.status();
    if !status.is_success() {
        return Err(http_error(status));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| GeaError::bad_gateway("GEA_MCP_INVALID_RESPONSE", "GEA MCP 响应读取失败"))?;
    let value = if content_type.starts_with("text/event-stream") {
        parse_sse_json(&bytes)?
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|_| GeaError::bad_gateway("GEA_MCP_INVALID_RESPONSE", "GEA MCP 返回了无效 JSON"))?
    };
    if let Some(error) = value.get("error") {
        let code = error
            .pointer("/data/businessCode")
            .or_else(|| error.pointer("/data/code"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("GEA_MCP_RPC_ERROR");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("GEA MCP 请求失败");
        return Err(mcp_business_error(code, message, error.get("data")));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| GeaError::bad_gateway("GEA_MCP_INVALID_RESPONSE", "GEA MCP 响应缺少 result"))
}

fn parse_sse_json(bytes: &[u8]) -> Result<Value, GeaError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| GeaError::bad_gateway("GEA_MCP_INVALID_RESPONSE", "GEA MCP SSE 不是 UTF-8"))?;
    let payload = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "[DONE]")
        .ok_or_else(|| GeaError::bad_gateway("GEA_MCP_INVALID_RESPONSE", "GEA MCP SSE 缺少 data"))?;
    serde_json::from_str(payload)
        .map_err(|_| GeaError::bad_gateway("GEA_MCP_INVALID_RESPONSE", "GEA MCP SSE data 不是有效 JSON"))
}

fn http_error(status: reqwest::StatusCode) -> GeaError {
    let status_code = status.as_u16();
    GeaError::from_http_status(status_code, format!("GEA_MCP_HTTP_{status_code}"), "GEA MCP 请求失败")
}

fn mcp_business_error(code: &str, message: &str, data: Option<&Value>) -> GeaError {
    let category = data.and_then(|data| data.get("category")).and_then(Value::as_str);
    let status = match code {
        "AI_SCHEMA_INVALID" => axum::http::StatusCode::BAD_REQUEST,
        "AI_GATEWAY_DATA_PERMISSION_DENIED" => axum::http::StatusCode::FORBIDDEN,
        "AI_GATEWAY_RESOURCE_URI_INVALID" | "AI_GATEWAY_RESOURCE_TOO_LARGE" => axum::http::StatusCode::BAD_REQUEST,
        "AI_GATEWAY_RESOURCE_NOT_FOUND" | "AI_GATEWAY_RESOURCE_EXPIRED" => axum::http::StatusCode::NOT_FOUND,
        "AI_GATEWAY_RESOURCE_FORBIDDEN" | "AI_GATEWAY_POLICY_CHANGED" => axum::http::StatusCode::FORBIDDEN,
        "AI_GATEWAY_RESOURCE_CANCELLED"
        | "AI_GATEWAY_SESSION_INVALID"
        | "AI_GATEWAY_SESSION_EXPIRED"
        | "AI_GATEWAY_SESSION_REVOKED" => axum::http::StatusCode::CONFLICT,
        "AI_GATEWAY_RESOURCE_STORAGE_UNAVAILABLE" => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        _ => axum::http::StatusCode::from_u16(super::status_for_category(category))
            .unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
    };
    let mut error = GeaError::new(status, code, message);
    // A JSON-RPC business error is not a transport failure. Only an explicit
    // gateway retryable flag permits retry; HTTP 200/-32000 says nothing about it.
    error.body.retryable = data.and_then(|data| data.get("retryable")).and_then(Value::as_bool) == Some(true);
    if let Some(data) = data {
        if let Some(category) = category.filter(|value| !value.is_empty()) {
            error.body.category = category.to_owned();
        }
        error.body.retry_after_ms = data.get("retryAfterMs").and_then(Value::as_u64);
        error.body.suggested_action = data.get("suggestedAction").and_then(Value::as_str).map(str::to_owned);
        error.body.request_id = data.get("requestId").and_then(Value::as_str).map(str::to_owned);
        error.body.trace_id = data.get("traceId").and_then(Value::as_str).map(str::to_owned);
        error.body.audit_id = data.get("auditId").and_then(Value::as_str).map(str::to_owned);
        error.body.details = data.get("details").cloned();
    }
    error
}

#[cfg(test)]
mod tests {
    use super::{parse_json_rpc_response, parse_sse_json};

    #[tokio::test]
    async fn deterministic_mcp_business_errors_are_not_retryable_upstream_failures() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for (code, category, status) in [
            ("AI_SCHEMA_INVALID", "VALIDATION", 400),
            ("AI_GATEWAY_DATA_PERMISSION_DENIED", "AUTHORIZATION", 403),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "error": { "code": -32603, "message": "request rejected",
                        "data": { "businessCode": code } }
                })))
                .expect(1)
                .mount(&server)
                .await;
            let response = reqwest::Client::new().post(server.uri()).send().await.unwrap();
            let error = parse_json_rpc_response(response).await.unwrap_err();
            assert_eq!(error.body.code, code);
            assert_eq!(error.status.as_u16(), status);
            assert_eq!(error.body.category, category);
            assert!(!error.body.retryable);
        }
    }

    #[tokio::test]
    async fn mcp_business_error_preserves_gateway_recovery_and_correlation_fields() {
        use axum::response::IntoResponse;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32000, "message": "quota reached", "data": {
                    "businessCode": "AI_GATEWAY_RATE_LIMITED", "category": "RATE_LIMIT", "retryable": true,
                    "retryAfterMs": 1500, "suggestedAction": "RETRY_AFTER_DELAY",
                    "requestId": "request-1", "traceId": "trace-1", "auditId": "audit-1",
                    "details": { "dimension": "consumer" }
                } }
            })))
            .mount(&server)
            .await;
        let response = reqwest::Client::new().post(server.uri()).send().await.unwrap();
        let error = parse_json_rpc_response(response).await.unwrap_err();
        assert_eq!(error.status.as_u16(), 429);
        let body = serde_json::to_value(&error.body).unwrap();
        assert_eq!(body["code"], "AI_GATEWAY_RATE_LIMITED");
        assert_eq!(body["category"], "RATE_LIMIT");
        assert_eq!(body["retryable"], true);
        assert_eq!(body["retryAfterMs"], 1500);
        assert_eq!(body["suggestedAction"], "RETRY_AFTER_DELAY");
        assert_eq!(body["requestId"], "request-1");
        assert_eq!(body["traceId"], "trace-1");
        assert_eq!(body["auditId"], "audit-1");
        assert_eq!(body["details"]["dimension"], "consumer");

        let response = error.into_response();
        assert_eq!(response.status().as_u16(), 429);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["details"]["suggestedAction"], "RETRY_AFTER_DELAY");
        assert_eq!(body["details"]["retryAfterMs"], 1500);
        assert_eq!(body["details"]["requestId"], "request-1");
        assert_eq!(body["details"]["upstream"]["dimension"], "consumer");
    }

    #[tokio::test]
    async fn mcp_business_error_retry_requires_an_explicit_boolean_even_for_upstream_categories() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for retryable in [None, Some(serde_json::json!(false)), Some(serde_json::json!("true"))] {
            server.reset().await;
            let mut data = serde_json::json!({
                "businessCode": "AI_GATEWAY_MCP_CALL_FAILED", "category": "UPSTREAM"
            });
            if let Some(retryable) = retryable {
                data["retryable"] = retryable;
            }
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "error": {"code": -32000, "message": "call failed", "data": data}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let response = reqwest::Client::new().post(server.uri()).send().await.unwrap();
            let error = parse_json_rpc_response(response).await.unwrap_err();
            assert_eq!(error.status.as_u16(), 502);
            assert_eq!(error.body.category, "UPSTREAM");
            assert!(!error.body.retryable);
        }
    }

    #[test]
    fn parses_json_rpc_sse_data() {
        let value = parse_sse_json(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true}}\n\n")
            .expect("parse SSE");
        assert_eq!(value["result"]["ok"], true);
    }
}
