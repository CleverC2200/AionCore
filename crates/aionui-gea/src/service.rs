use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aionui_api_types::{
    CreateGeaSessionRequest, GeaAuthSessionStatus, GeaSessionResponse, GeaToolCallResponse, GeaToolInfo,
    SetGeaAuthSessionRequest,
};
use axum::http::StatusCode;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::GeaError;

const DEFAULT_GEA_BASE_URL: &str = "https://gea.synear.cn/gea-boot";
const GEA_CONTEXT_FIELDS: &[&str] = &[
    "agentcode",
    "auditid",
    "authorizationrevision",
    "channel",
    "consumercode",
    "consumertype",
    "conversationid",
    "delegationtoken",
    "mcpcode",
    "principalid",
    "principaltype",
    "requestid",
    "sessionid",
    "tenantid",
    "toolname",
    "traceid",
    "userid",
];

#[derive(Clone)]
struct GeaCredential {
    access_token: Arc<str>,
    tenant_id: Option<String>,
}

#[derive(Clone)]
struct GeaConversationSession {
    agent_code: String,
    session_id: String,
    conversation_id: String,
    delegation_token: Arc<str>,
    tools: HashMap<String, GeaToolInfo>,
}

/// A per-process GEA gateway. Credentials and delegation tokens deliberately
/// remain private and are indexed by the authenticated AionCore user.
pub struct GeaService {
    client: reqwest::Client,
    base_url: String,
    credentials: RwLock<HashMap<String, GeaCredential>>,
    reauth_required: RwLock<HashSet<String>>,
    sessions: RwLock<HashMap<(String, String), GeaConversationSession>>,
}

impl GeaService {
    pub fn from_env() -> Result<Self, GeaError> {
        let base_url = std::env::var("AIONUI_GEA_BASE_URL").unwrap_or_else(|_| DEFAULT_GEA_BASE_URL.to_owned());
        Self::new(reqwest::Client::new(), base_url)
    }

    pub fn new(client: reqwest::Client, base_url: impl Into<String>) -> Result<Self, GeaError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        if !(base_url.starts_with("https://") || cfg!(test) && base_url.starts_with("http://")) {
            return Err(GeaError::invalid_request("GEA 地址必须使用 HTTPS"));
        }
        Ok(Self {
            client,
            base_url,
            credentials: RwLock::new(HashMap::new()),
            reauth_required: RwLock::new(HashSet::new()),
            sessions: RwLock::new(HashMap::new()),
        })
    }

    pub async fn set_auth_session(
        &self,
        user_id: &str,
        request: SetGeaAuthSessionRequest,
    ) -> Result<GeaAuthSessionStatus, GeaError> {
        let access_token = request.access_token.trim();
        if access_token.is_empty() {
            return Err(GeaError::invalid_request("GEA access token 不能为空"));
        }
        let tenant_id = request.tenant_id.and_then(non_empty);
        self.credentials.write().await.insert(
            user_id.to_owned(),
            GeaCredential {
                access_token: Arc::from(access_token),
                tenant_id: tenant_id.clone(),
            },
        );
        self.reauth_required.write().await.remove(user_id);
        self.clear_sessions(user_id).await;
        Ok(GeaAuthSessionStatus {
            authenticated: true,
            reauth_required: false,
            tenant_id,
        })
    }

    pub async fn auth_status(&self, user_id: &str) -> GeaAuthSessionStatus {
        let credential = self.credentials.read().await.get(user_id).cloned();
        match credential {
            Some(value) => GeaAuthSessionStatus {
                authenticated: true,
                reauth_required: false,
                tenant_id: value.tenant_id,
            },
            None => GeaAuthSessionStatus {
                authenticated: false,
                reauth_required: self.reauth_required.read().await.contains(user_id),
                tenant_id: None,
            },
        }
    }

    pub async fn clear_auth_session(&self, user_id: &str) {
        self.credentials.write().await.remove(user_id);
        self.reauth_required.write().await.remove(user_id);
        self.clear_sessions(user_id).await;
    }

    async fn invalidate_auth_session(&self, user_id: &str) {
        self.credentials.write().await.remove(user_id);
        self.reauth_required.write().await.insert(user_id.to_owned());
        self.clear_sessions(user_id).await;
    }

    pub async fn create_session(
        &self,
        user_id: &str,
        conversation_id: &str,
        request: CreateGeaSessionRequest,
    ) -> Result<GeaSessionResponse, GeaError> {
        let consumer_code = request.consumer_code.trim();
        if consumer_code.is_empty() || conversation_id.trim().is_empty() {
            return Err(GeaError::invalid_request("consumerCode 和 conversationId 不能为空"));
        }
        let credential = self.credential(user_id).await?;
        let request_id = Uuid::now_v7().to_string();
        let mut body = json!({
            "consumerType": "AGENT",
            "consumerCode": consumer_code,
            "requestId": request_id,
            "conversationId": conversation_id,
            "channel": "AION_CORE"
        });
        if let Some(preparation_id) = request.preparation_id.and_then(non_empty) {
            body["preparationId"] = Value::String(preparation_id);
        }

        let value = self
            .post_for_user(user_id, &credential, "/ai/gateway/session", &body)
            .await?;
        let result = value
            .get("result")
            .ok_or_else(|| invalid_upstream("GEA Session 响应缺少 result"))?;
        let context = result
            .get("gatewayContext")
            .ok_or_else(|| invalid_upstream("GEA Session 响应缺少 gatewayContext"))?;
        let session_id = required_string(context, "sessionId")?;
        let returned_conversation_id = required_string(context, "conversationId")?;
        if returned_conversation_id != conversation_id {
            return Err(invalid_upstream("GEA Session 返回了不匹配的 conversationId"));
        }
        let returned_agent_code = context
            .get("agentId")
            .or_else(|| context.get("consumerCode"))
            .and_then(Value::as_str)
            .unwrap_or(consumer_code);
        if returned_agent_code != consumer_code {
            return Err(invalid_upstream("GEA Session 返回了不匹配的 consumerCode"));
        }
        let allowed = result
            .pointer("/accessDecision/allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allowed {
            return Err(upstream_business_error(&value, StatusCode::FORBIDDEN));
        }
        let delegation_token = required_string(result, "delegationToken")?;
        let effective_capability_codes = result
            .get("effectiveCapabilityCodes")
            .and_then(Value::as_array)
            .map(|values| values.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();

        self.sessions.write().await.insert(
            (user_id.to_owned(), conversation_id.to_owned()),
            GeaConversationSession {
                agent_code: consumer_code.to_owned(),
                session_id: session_id.clone(),
                conversation_id: returned_conversation_id.clone(),
                delegation_token: Arc::from(delegation_token),
                tools: HashMap::new(),
            },
        );
        tracing::info!(
            user_id,
            conversation_id,
            consumer_code,
            request_id,
            "GEA gateway session created"
        );
        Ok(GeaSessionResponse {
            session_id,
            conversation_id: returned_conversation_id,
            consumer_code: consumer_code.to_owned(),
            effective_capability_codes,
        })
    }

    pub async fn list_tools(&self, user_id: &str, conversation_id: &str) -> Result<Vec<GeaToolInfo>, GeaError> {
        let credential = self.credential(user_id).await?;
        let session = self.session(user_id, conversation_id).await?;
        let body = session.gateway_body();
        let value = self
            .post_for_conversation(
                user_id,
                conversation_id,
                &credential,
                "/ai/gateway/mcp/proxy/list",
                &body,
            )
            .await?;
        let raw_tools = value
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_upstream("GEA Tool 列表响应缺少 tools"))?;
        let mut tools = Vec::with_capacity(raw_tools.len());
        let mut names = HashSet::with_capacity(raw_tools.len());
        for raw in raw_tools {
            let name = required_string(raw, "name")?;
            if !names.insert(name.clone()) {
                return Err(invalid_upstream("GEA Tool 列表存在重名工具"));
            }
            let source_code = required_string(raw, "sourceCode")?;
            let input_schema = sanitize_tool_input_schema(
                raw.get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            )?;
            tools.push(GeaToolInfo {
                name,
                source_code,
                description: raw
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input_schema,
            });
        }
        let tools_by_name = tools.iter().cloned().map(|tool| (tool.name.clone(), tool)).collect();
        if let Some(stored) = self
            .sessions
            .write()
            .await
            .get_mut(&(user_id.to_owned(), conversation_id.to_owned()))
        {
            stored.tools = tools_by_name;
        }
        Ok(tools)
    }

    pub async fn call_tool(
        &self,
        user_id: &str,
        conversation_id: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<GeaToolCallResponse, GeaError> {
        if !arguments.is_object() && !arguments.is_null() {
            return Err(GeaError::invalid_request("arguments 必须是 JSON object"));
        }
        let credential = self.credential(user_id).await?;
        let mut session = self.session(user_id, conversation_id).await?;
        if !session.tools.contains_key(tool_name) {
            self.list_tools(user_id, conversation_id).await?;
            session = self.session(user_id, conversation_id).await?;
        }
        let tool = session
            .tools
            .get(tool_name)
            .cloned()
            .ok_or_else(|| GeaError::tool_not_found(tool_name))?;
        let body = json!({
            "agentCode": session.agent_code,
            "sessionId": session.session_id,
            "conversationId": session.conversation_id,
            "delegationToken": session.delegation_token.as_ref(),
            "mcpCode": tool.source_code,
            "toolName": tool.name,
            "arguments": if arguments.is_null() { json!({}) } else { arguments }
        });
        let started = Instant::now();
        let value = self
            .post_for_conversation(
                user_id,
                conversation_id,
                &credential,
                "/ai/gateway/mcp/proxy/call",
                &body,
            )
            .await?;
        if value.get("sourceCode").and_then(Value::as_str) != Some(tool.source_code.as_str())
            || value.get("toolName").and_then(Value::as_str) != Some(tool.name.as_str())
        {
            return Err(invalid_upstream("GEA Tool 调用响应与请求不匹配"));
        }
        let audit_id = value.get("auditId").and_then(Value::as_str).and_then(non_empty);
        tracing::info!(
            user_id,
            conversation_id,
            tool_name,
            audit_id,
            duration_ms = started.elapsed().as_millis(),
            "GEA tool call completed"
        );
        Ok(GeaToolCallResponse {
            result: value.get("result").cloned().unwrap_or(Value::Null),
            audit_id,
        })
    }

    async fn credential(&self, user_id: &str) -> Result<GeaCredential, GeaError> {
        self.credentials
            .read()
            .await
            .get(user_id)
            .cloned()
            .ok_or_else(GeaError::unauthenticated)
    }

    async fn session(&self, user_id: &str, conversation_id: &str) -> Result<GeaConversationSession, GeaError> {
        self.sessions
            .read()
            .await
            .get(&(user_id.to_owned(), conversation_id.to_owned()))
            .cloned()
            .ok_or_else(GeaError::session_required)
    }

    async fn clear_sessions(&self, user_id: &str) {
        self.sessions.write().await.retain(|(owner, _), _| owner != user_id);
    }

    async fn post_for_user(
        &self,
        user_id: &str,
        credential: &GeaCredential,
        path: &str,
        body: &Value,
    ) -> Result<Value, GeaError> {
        let result = self.post(credential, path, body).await.and_then(|value| {
            ensure_success(&value)?;
            Ok(value)
        });
        if matches!(&result, Err(error) if error.status == StatusCode::UNAUTHORIZED) {
            self.invalidate_auth_session(user_id).await;
        }
        result
    }

    async fn post_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
        credential: &GeaCredential,
        path: &str,
        body: &Value,
    ) -> Result<Value, GeaError> {
        let result = self.post_for_user(user_id, credential, path, body).await;
        if matches!(&result, Err(error) if error.body.category == "SESSION") {
            self.sessions
                .write()
                .await
                .remove(&(user_id.to_owned(), conversation_id.to_owned()));
        }
        result
    }

    async fn post(&self, credential: &GeaCredential, path: &str, body: &Value) -> Result<Value, GeaError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-access-token",
            HeaderValue::from_str(credential.access_token.as_ref())
                .map_err(|_| GeaError::invalid_request("GEA access token 格式无效"))?,
        );
        if let Some(tenant_id) = credential.tenant_id.as_deref() {
            headers.insert(
                "x-tenant-id",
                HeaderValue::from_str(tenant_id).map_err(|_| GeaError::invalid_request("GEA tenantId 格式无效"))?,
            );
        }
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|_| GeaError::new(StatusCode::BAD_GATEWAY, "GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
        let status = response.status();
        let retry_after_ms = parse_retry_after_ms(response.headers());
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| invalid_upstream("GEA 返回了无效 JSON"))?;
        if !status.is_success() {
            let mut error = upstream_business_error(&value, status);
            error.body.retry_after_ms = retry_after_ms;
            return Err(error);
        }
        Ok(value)
    }
}

impl GeaConversationSession {
    fn gateway_body(&self) -> Value {
        json!({
            "agentCode": self.agent_code,
            "sessionId": self.session_id,
            "conversationId": self.conversation_id,
            "delegationToken": self.delegation_token.as_ref()
        })
    }
}

fn non_empty(value: impl AsRef<str>) -> Option<String> {
    let value = value.as_ref().trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn required_string(value: &Value, field: &str) -> Result<String, GeaError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(non_empty)
        .ok_or_else(|| invalid_upstream(format!("GEA 响应缺少 {field}")))
}

fn ensure_success(value: &Value) -> Result<(), GeaError> {
    if value.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(upstream_business_error(value, StatusCode::OK))
    }
}

fn sanitize_tool_input_schema(mut schema: Value) -> Result<Value, GeaError> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(invalid_upstream("GEA Tool inputSchema 根节点必须为 object"));
    }
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.retain(|name, _| !is_gea_context_field(name));
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|name| name.as_str().is_none_or(|name| !is_gea_context_field(name)));
    }
    Ok(schema)
}

fn is_gea_context_field(name: &str) -> bool {
    let normalized = name
        .chars()
        .filter(|value| value.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    GEA_CONTEXT_FIELDS.contains(&normalized.as_str())
}

fn invalid_upstream(message: impl Into<String>) -> GeaError {
    GeaError::new(StatusCode::BAD_GATEWAY, "GEA_INVALID_RESPONSE", message)
}

fn upstream_business_error(value: &Value, fallback_status: StatusCode) -> GeaError {
    let code = value
        .get("code")
        .and_then(value_as_string)
        .unwrap_or_else(|| "GEA_UPSTREAM_ERROR".to_owned());
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .and_then(non_empty)
        .unwrap_or_else(|| "GEA 请求未完成".to_owned());
    let category = value.get("category").and_then(Value::as_str).and_then(non_empty);
    let status = if fallback_status.is_success() {
        status_for_category(category.as_deref())
    } else {
        fallback_status
    };
    let mut error = GeaError::new(status, code, message);
    if let Some(category) = category {
        error.body.category = category;
    }
    error.body.retryable = value
        .get("retryable")
        .and_then(Value::as_bool)
        .unwrap_or(error.body.retryable);
    error.body.retry_after_ms = value.get("retryAfterMs").and_then(Value::as_u64);
    error.body.request_id = value.get("requestId").and_then(Value::as_str).and_then(non_empty);
    error.body.trace_id = value.get("traceId").and_then(Value::as_str).and_then(non_empty);
    error.body.audit_id = value.get("auditId").and_then(Value::as_str).and_then(non_empty);
    error.body.details = value.get("details").cloned();
    error
}

fn status_for_category(category: Option<&str>) -> StatusCode {
    match category {
        Some("AUTHENTICATION") => StatusCode::UNAUTHORIZED,
        Some("AUTHORIZATION") => StatusCode::FORBIDDEN,
        Some("SESSION") | Some("CONFLICT") => StatusCode::CONFLICT,
        Some("VALIDATION") => StatusCode::UNPROCESSABLE_ENTITY,
        Some("RATE_LIMIT") => StatusCode::TOO_MANY_REQUESTS,
        Some("UPSTREAM") => StatusCode::BAD_GATEWAY,
        _ => StatusCode::BAD_GATEWAY,
    }
}

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => non_empty(value),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use aionui_api_types::{CreateGeaSessionRequest, SetGeaAuthSessionRequest};
    use serde_json::json;
    use wiremock::matchers::{body_json, body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::GeaService;

    async fn authenticated_service(server: &MockServer) -> GeaService {
        let service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service
            .set_auth_session(
                "user-1",
                SetGeaAuthSessionRequest {
                    access_token: "test-access-token".to_owned(),
                    tenant_id: Some("tenant-1".to_owned()),
                },
            )
            .await
            .unwrap();
        service
    }

    async fn mount_session(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .and(header("x-access-token", "test-access-token"))
            .and(header("x-tenant-id", "tenant-1"))
            .and(body_partial_json(json!({
                "consumerType": "AGENT",
                "consumerCode": "agent-sales",
                "conversationId": "conversation-1",
                "channel": "AION_CORE",
                "preparationId": "preparation-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "accessDecision": { "allowed": true },
                    "delegationToken": "delegation-secret",
                    "effectiveCapabilityCodes": ["MCP_TOOL:cube:query_business_data"],
                    "gatewayContext": {
                        "consumerCode": "agent-sales",
                        "sessionId": "gea-session-1",
                        "conversationId": "conversation-1"
                    }
                }
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn create_session(service: &GeaService) {
        service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: Some("preparation-1".to_owned()),
                },
            )
            .await
            .unwrap();
    }

    async fn mount_query_tool(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "tools": [{
                    "name": "query_business_data",
                    "sourceCode": "cube",
                    "description": "Query business data",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "queries": { "type": "array" }
                        },
                        "required": ["queries"]
                    }
                }]
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn unified_session_uses_agent_consumer_and_preparation() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        let service = authenticated_service(&server).await;

        let response = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: Some("preparation-1".to_owned()),
                },
            )
            .await
            .unwrap();

        assert_eq!(response.session_id, "gea-session-1");
        assert_eq!(response.conversation_id, "conversation-1");
        assert_eq!(response.consumer_code, "agent-sales");
    }

    #[tokio::test]
    async fn tool_call_keeps_gateway_context_out_of_business_arguments() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/list"))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "sessionId": "gea-session-1",
                "conversationId": "conversation-1",
                "delegationToken": "delegation-secret"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "tools": [{
                    "name": "query_business_data",
                    "sourceCode": "cube",
                    "description": "Query business data",
                    "inputSchema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "queries": { "type": "array" },
                            "sessionId": { "type": "string" },
                            "delegation_token": { "type": "string" }
                        },
                        "required": ["queries", "sessionId", "delegation_token"]
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .and(body_json(json!({
                "agentCode": "agent-sales",
                "sessionId": "gea-session-1",
                "conversationId": "conversation-1",
                "delegationToken": "delegation-secret",
                "mcpCode": "cube",
                "toolName": "query_business_data",
                "arguments": {
                    "queries": [{ "measures": ["sales"] }]
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "sourceCode": "cube",
                "toolName": "query_business_data",
                "auditId": "audit-1",
                "result": { "rows": [] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let service = authenticated_service(&server).await;
        create_session(&service).await;
        let tools = service.list_tools("user-1", "conversation-1").await.unwrap();
        assert_eq!(tools[0].input_schema["required"], json!(["queries"]));
        assert_eq!(
            tools[0].input_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["queries"]
        );
        let response = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [{ "measures": ["sales"] }] }),
            )
            .await
            .unwrap();

        assert_eq!(response.audit_id.as_deref(), Some("audit-1"));
        assert_eq!(response.result, json!({ "rows": [] }));
    }

    #[test]
    fn context_only_tool_schema_becomes_an_empty_business_object() {
        let schema = super::sanitize_tool_input_schema(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agentCode": { "type": "string" },
                "conversation_id": { "type": "string" },
                "mcpCode": { "type": "string" }
            },
            "required": ["agentCode", "conversation_id", "mcpCode"]
        }))
        .unwrap();

        assert_eq!(schema["properties"], json!({}));
        assert_eq!(schema["required"], json!([]));
    }

    #[tokio::test]
    async fn successful_http_business_error_maps_from_gea_category() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false,
                "code": "AI_GATEWAY_DATA_PERMISSION_DENIED",
                "message": "Capability data governance is incomplete",
                "category": "AUTHORIZATION",
                "retryable": false,
                "requestId": "request-1",
                "traceId": "trace-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let error = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(error.body.code, "AI_GATEWAY_DATA_PERMISSION_DENIED");
        assert_eq!(error.body.request_id.as_deref(), Some("request-1"));
        assert!(service.auth_status("user-1").await.authenticated);
    }

    #[tokio::test]
    async fn upstream_authentication_failure_clears_cached_credentials_and_sessions() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/session"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false,
                "code": "401",
                "message": "Token expired",
                "category": "AUTHENTICATION",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;

        let error = service
            .create_session(
                "user-1",
                "conversation-1",
                CreateGeaSessionRequest {
                    consumer_code: "agent-sales".to_owned(),
                    preparation_id: None,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
        let status = service.auth_status("user-1").await;
        assert!(!status.authenticated);
        assert!(status.reauth_required);
        assert_eq!(
            service
                .create_session(
                    "user-1",
                    "conversation-1",
                    CreateGeaSessionRequest {
                        consumer_code: "agent-sales".to_owned(),
                        preparation_id: None,
                    },
                )
                .await
                .unwrap_err()
                .body
                .code,
            "GEA_AUTH_REQUIRED"
        );

        service
            .set_auth_session(
                "user-1",
                SetGeaAuthSessionRequest {
                    access_token: "replacement-access-token".to_owned(),
                    tenant_id: Some("tenant-1".to_owned()),
                },
            )
            .await
            .unwrap();
        let replacement_status = service.auth_status("user-1").await;
        assert!(replacement_status.authenticated);
        assert!(!replacement_status.reauth_required);
    }

    #[tokio::test]
    async fn tool_authentication_failure_clears_cached_credentials_and_sessions() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false,
                "code": "GEA_TOKEN_EXPIRED",
                "message": "Token expired",
                "category": "AUTHENTICATION",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
        let status = service.auth_status("user-1").await;
        assert!(!status.authenticated);
        assert!(status.reauth_required);
        let session_error = match service.session("user-1", "conversation-1").await {
            Ok(_) => panic!("authentication failure must discard the cached session"),
            Err(error) => error,
        };
        assert_eq!(session_error.body.code, "GEA_SESSION_REQUIRED");
    }

    #[tokio::test]
    async fn session_failure_discards_only_the_expired_conversation_session() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "success": false,
                "code": "GEA_SESSION_STALE",
                "message": "Session authorization revision is stale",
                "category": "SESSION",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::CONFLICT);
        assert!(service.auth_status("user-1").await.authenticated);
        let session_error = match service.session("user-1", "conversation-1").await {
            Ok(_) => panic!("session failure must discard the expired conversation session"),
            Err(error) => error,
        };
        assert_eq!(session_error.body.code, "GEA_SESSION_REQUIRED");
    }

    #[tokio::test]
    async fn rate_limit_preserves_retry_after_and_the_current_session() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "2")
                    .set_body_json(json!({
                        "success": false,
                        "code": "AI_GATEWAY_RATE_LIMITED",
                        "message": "Rate limited",
                        "category": "RATE_LIMIT",
                        "retryable": true,
                        "retryAfterMs": 500
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.body.code, "AI_GATEWAY_RATE_LIMITED");
        assert_eq!(error.body.category, "RATE_LIMIT");
        assert!(error.body.retryable);
        assert_eq!(error.body.retry_after_ms, Some(2_000));
        assert!(service.auth_status("user-1").await.authenticated);
        assert!(service.session("user-1", "conversation-1").await.is_ok());
    }

    #[tokio::test]
    async fn business_conflict_preserves_the_current_session() {
        let server = MockServer::start().await;
        mount_session(&server).await;
        mount_query_tool(&server).await;
        Mock::given(method("POST"))
            .and(path("/ai/gateway/mcp/proxy/call"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "success": false,
                "code": "GEA_IDEMPOTENCY_CONFLICT",
                "message": "Conflicting request",
                "category": "CONFLICT",
                "retryable": false
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = authenticated_service(&server).await;
        create_session(&service).await;
        service.list_tools("user-1", "conversation-1").await.unwrap();

        let error = service
            .call_tool(
                "user-1",
                "conversation-1",
                "query_business_data",
                json!({ "queries": [] }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(error.body.category, "CONFLICT");
        assert!(service.session("user-1", "conversation-1").await.is_ok());
    }
}
