//! Conversation-scoped GEA MCP stdio server.
//!
//! The agent process receives only the normal AionCore runtime context. This
//! helper calls authenticated local AionCore routes; it never receives the GEA
//! login token or delegation token.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::{ErrorData as McpError, ServerHandler, service::ServiceExt, transport};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";
const ENV_AGENT_CODE: &str = "AIONUI_GEA_AGENT_CODE";
const DEFAULT_AGENT_CODE: &str = "sales_forecast";
const MAX_TOOL_NAME_LENGTH: usize = 64;
const GEA_IDENTITY_BOOTSTRAP_TOOL: &str = "gateway.session.currentUser.resolve";

pub async fn run_gea_stdio() -> ExitCode {
    let env = match GeaStdioEnv::from_env() {
        Ok(env) => env,
        Err(code) => {
            eprintln!("GEA_MCP_ENV_INVALID field={code}");
            return ExitCode::FAILURE;
        }
    };
    let server = GeaStdioServer {
        client: reqwest::Client::new(),
        env,
        session_ready: Arc::new(Mutex::new(false)),
        tools: Arc::new(RwLock::new(HashMap::new())),
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => match peer.waiting().await {
            Ok(_) => ExitCode::SUCCESS,
            Err(_) => {
                eprintln!("GEA_MCP_SESSION_FAILED");
                ExitCode::FAILURE
            }
        },
        Err(_) => {
            eprintln!("GEA_MCP_START_FAILED");
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone)]
struct GeaStdioEnv {
    base_url: String,
    conversation_id: String,
    user_id: String,
    runtime_token: String,
    agent_code: String,
}

impl GeaStdioEnv {
    fn from_env() -> Result<Self, &'static str> {
        Ok(Self {
            base_url: required_env(ENV_BASE_URL)?.trim_end_matches('/').to_owned(),
            conversation_id: required_env(ENV_CONVERSATION_ID)?,
            user_id: required_env(ENV_USER_ID)?,
            runtime_token: required_env(ENV_RUNTIME_TOKEN)?,
            agent_code: optional_env(ENV_AGENT_CODE).unwrap_or_else(|| DEFAULT_AGENT_CODE.to_owned()),
        })
    }
}

fn required_env(name: &'static str) -> Result<String, &'static str> {
    optional_env(name).ok_or(name)
}

fn optional_env(name: &'static str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[derive(Clone)]
struct GeaStdioServer {
    client: reqwest::Client,
    env: GeaStdioEnv,
    session_ready: Arc<Mutex<bool>>,
    tools: Arc<RwLock<HashMap<String, ToolInfo>>>,
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
    data: T,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiErrorBody {
    code: String,
    message: String,
    category: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolInfo {
    name: String,
    #[allow(dead_code)]
    source_code: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    input_schema: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallResponse {
    result: Value,
}

impl GeaStdioServer {
    async fn ensure_session(&self) -> Result<(), McpError> {
        let mut ready = self.session_ready.lock().await;
        if *ready {
            return Ok(());
        }
        let path = format!(
            "/api/gea/conversations/{}/session",
            encode_path_segment(&self.env.conversation_id)
        );
        self.request::<Value>(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "consumerCode": self.env.agent_code
            })),
        )
        .await?;
        *ready = true;
        Ok(())
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, McpError> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.env.base_url, path))
            .header("content-type", "application/json")
            .header("x-aionui-conversation-id", &self.env.conversation_id)
            .header("x-aionui-user-id", &self.env.user_id)
            .header("x-aionui-runtime-token", &self.env.runtime_token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| McpError::internal_error("GEA_MCP_BACKEND_UNAVAILABLE", None))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| McpError::internal_error("GEA_MCP_BACKEND_RESPONSE_INVALID", None))?;
        if !status.is_success() {
            return Err(backend_mcp_error(status, &bytes));
        }
        let envelope = serde_json::from_slice::<ApiEnvelope<T>>(&bytes)
            .map_err(|_| McpError::internal_error("GEA_MCP_BACKEND_RESPONSE_INVALID", None))?;
        Ok(envelope.data)
    }

    async fn load_tools(&self) -> Result<Vec<(String, ToolInfo)>, McpError> {
        self.ensure_session().await?;
        let path = format!(
            "/api/gea/conversations/{}/tools",
            encode_path_segment(&self.env.conversation_id)
        );
        let tools: Vec<ToolInfo> = match self.request(reqwest::Method::GET, &path, None).await {
            Ok(tools) => tools,
            Err(error) => {
                if should_reset_session_after_error(&error) {
                    *self.session_ready.lock().await = false;
                }
                return Err(error);
            }
        };
        let mut used_names = HashSet::with_capacity(tools.len());
        let exposed = tools
            .into_iter()
            .filter(|tool| should_expose_tool_to_agent(&tool.name))
            .map(|tool| {
                let name = compatible_tool_name(&tool.name, &used_names);
                used_names.insert(name.clone());
                (name, tool)
            })
            .collect::<Vec<_>>();
        *self.tools.write().await = exposed.iter().cloned().collect();
        Ok(exposed)
    }
}

fn backend_mcp_error(status: reqwest::StatusCode, body: &[u8]) -> McpError {
    let Ok(error) = serde_json::from_slice::<ApiErrorBody>(body) else {
        return McpError::internal_error(format!("GEA_MCP_BACKEND_HTTP_{}", status.as_u16()), None);
    };
    let message = format!(
        "{}: {} [category={} retryable={}]",
        error.code, error.message, error.category, error.retryable
    );
    let data = serde_json::to_value(error).ok();
    McpError::internal_error(message, data)
}

fn should_reset_session_after_error(error: &McpError) -> bool {
    matches!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("category"))
            .and_then(Value::as_str),
        None | Some("AUTHENTICATION" | "SESSION" | "UPSTREAM" | "INTERNAL")
    )
}

impl ServerHandler for GeaStdioServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("GEA enterprise tools scoped to the current AionUi conversation")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools =
            self.load_tools()
                .await?
                .into_iter()
                .map(|(exposed_name, tool)| {
                    let input_schema =
                        tool.input_schema.as_object().cloned().unwrap_or_else(|| {
                            Map::from_iter([("type".to_owned(), Value::String("object".to_owned()))])
                        });
                    Tool::new_with_raw(
                        exposed_name,
                        (!tool.description.is_empty()).then_some(Cow::Owned(tool.description)),
                        Arc::new(input_schema),
                    )
                })
                .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_session().await?;
        let exposed_name = request.name.into_owned();
        let mut tool = self.tools.read().await.get(&exposed_name).cloned();
        if tool.is_none() {
            self.load_tools().await?;
            tool = self.tools.read().await.get(&exposed_name).cloned();
        }
        let tool = tool.ok_or_else(|| McpError::invalid_params("GEA_MCP_TOOL_NOT_FOUND", None))?;
        let path = format!(
            "/api/gea/conversations/{}/tools/{}",
            encode_path_segment(&self.env.conversation_id),
            encode_path_segment(&tool.name)
        );
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let response: ToolCallResponse = match self
            .request(reqwest::Method::POST, &path, Some(json!({ "arguments": arguments })))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if should_reset_session_after_error(&error) {
                    *self.session_ready.lock().await = false;
                }
                return Err(error);
            }
        };
        let text = match &response.result {
            Value::String(value) => value.clone(),
            value => serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
        };
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        if response.result.is_object() {
            result.structured_content = Some(response.result);
        }
        Ok(result)
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn compatible_tool_name(name: &str, used_names: &HashSet<String>) -> String {
    let sanitized = name
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '_' | '-') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized = if sanitized.is_empty() {
        "gea_tool".to_owned()
    } else {
        sanitized
    };
    if sanitized.len() <= MAX_TOOL_NAME_LENGTH && !used_names.contains(&sanitized) {
        return sanitized;
    }
    for attempt in 0_u32.. {
        let digest = Sha256::digest(format!("{name}\0{attempt}"));
        let suffix = digest[..4].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let prefix_len = MAX_TOOL_NAME_LENGTH - suffix.len() - 1;
        let prefix = sanitized.chars().take(prefix_len).collect::<String>();
        let candidate = format!("{prefix}_{suffix}");
        if !used_names.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn should_expose_tool_to_agent(name: &str) -> bool {
    name != GEA_IDENTITY_BOOTSTRAP_TOOL
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::{Mutex, RwLock};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        GeaStdioEnv, GeaStdioServer, backend_mcp_error, compatible_tool_name, should_expose_tool_to_agent,
        should_reset_session_after_error,
    };

    #[test]
    fn agent_projection_excludes_gateway_identity_bootstrap_tool() {
        assert!(!should_expose_tool_to_agent("gateway.session.currentUser.resolve"));
        assert!(should_expose_tool_to_agent("query_business_data"));
    }

    #[tokio::test]
    async fn loaded_agent_tools_keep_business_tools_and_hide_identity_bootstrap() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/gea/conversations/conversation-1/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/gea/conversations/conversation-1/tools"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "name": "query_business_data",
                        "sourceCode": "cube",
                        "description": "Query business data",
                        "inputSchema": { "type": "object" }
                    },
                    {
                        "name": "gateway.session.currentUser.resolve",
                        "sourceCode": "current-user",
                        "description": "Resolve current user",
                        "inputSchema": { "type": "object" }
                    }
                ]
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let server = GeaStdioServer {
            client: reqwest::Client::new(),
            env: GeaStdioEnv {
                base_url: upstream.uri(),
                conversation_id: "conversation-1".to_owned(),
                user_id: "user-1".to_owned(),
                runtime_token: "runtime-token".to_owned(),
                agent_code: "sales_forecast".to_owned(),
            },
            session_ready: Arc::new(Mutex::new(false)),
            tools: Arc::new(RwLock::new(HashMap::new())),
        };

        let tools = server.load_tools().await.expect("load tools");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].0, "query_business_data");
        assert_eq!(tools[0].1.name, "query_business_data");
    }

    #[tokio::test]
    async fn authorization_failure_keeps_the_ready_session() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/gea/conversations/conversation-1/tools"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "code": "AI_GATEWAY_DATA_PERMISSION_DENIED",
                "message": "能力未完成数据治理分类",
                "category": "AUTHORIZATION",
                "retryable": false
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let server = GeaStdioServer {
            client: reqwest::Client::new(),
            env: GeaStdioEnv {
                base_url: upstream.uri(),
                conversation_id: "conversation-1".to_owned(),
                user_id: "user-1".to_owned(),
                runtime_token: "runtime-token".to_owned(),
                agent_code: "sales_forecast".to_owned(),
            },
            session_ready: Arc::new(Mutex::new(true)),
            tools: Arc::new(RwLock::new(HashMap::new())),
        };

        let error = match server.load_tools().await {
            Ok(_) => panic!("authorization must fail"),
            Err(error) => error,
        };

        assert_eq!(
            error.data.as_ref().and_then(|data| data["category"].as_str()),
            Some("AUTHORIZATION")
        );
        assert!(*server.session_ready.lock().await);
    }

    #[test]
    fn tool_names_are_mcp_safe_and_collisions_remain_distinct() {
        let mut used = HashSet::new();
        let first = compatible_tool_name("sales.query", &used);
        used.insert(first.clone());
        let second = compatible_tool_name("sales/query", &used);

        assert_eq!(first, "sales_query");
        assert_ne!(second, first);
        assert!(second.len() <= 64);
        assert!(
            second
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-'))
        );
    }

    #[test]
    fn backend_error_preserves_gea_code_and_retry_contract() {
        let error = backend_mcp_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{
                "code":"AI_GATEWAY_DATA_PERMISSION_DENIED",
                "message":"能力未完成数据治理分类",
                "category":"AUTHORIZATION",
                "retryable":false,
                "requestId":"request-1",
                "traceId":"trace-1"
            }"#
            .as_bytes(),
        );
        let value = serde_json::to_value(&error).expect("serialize MCP error");

        assert_eq!(
            value["message"],
            "AI_GATEWAY_DATA_PERMISSION_DENIED: 能力未完成数据治理分类 [category=AUTHORIZATION retryable=false]"
        );
        assert_eq!(value["data"]["code"], "AI_GATEWAY_DATA_PERMISSION_DENIED");
        assert_eq!(value["data"]["retryable"], false);
        assert_eq!(value["data"]["requestId"], "request-1");
        assert_eq!(value["data"]["traceId"], "trace-1");
    }

    #[test]
    fn backend_rate_limit_error_preserves_retry_delay() {
        let error = backend_mcp_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{
                "code":"AI_GATEWAY_RATE_LIMITED",
                "message":"请求过于频繁",
                "category":"RATE_LIMIT",
                "retryable":true,
                "retryAfterMs":2000
            }"#
            .as_bytes(),
        );
        let value = serde_json::to_value(&error).expect("serialize MCP error");

        assert_eq!(value["data"]["code"], "AI_GATEWAY_RATE_LIMITED");
        assert_eq!(value["data"]["category"], "RATE_LIMIT");
        assert_eq!(value["data"]["retryable"], true);
        assert_eq!(value["data"]["retryAfterMs"], 2000);
        assert!(!should_reset_session_after_error(&error));
    }

    #[test]
    fn session_and_upstream_failures_require_session_recovery() {
        for category in ["AUTHENTICATION", "SESSION", "UPSTREAM", "INTERNAL"] {
            let error = backend_mcp_error(
                reqwest::StatusCode::CONFLICT,
                json!({
                    "code": "GEA_RECOVERY_REQUIRED",
                    "message": "recovery required",
                    "category": category,
                    "retryable": false
                })
                .to_string()
                .as_bytes(),
            );

            assert!(should_reset_session_after_error(&error), "category={category}");
        }
    }
}
