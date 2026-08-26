//! Conversation-scoped GEA MCP stdio server.
//!
//! The agent process receives only the normal AionCore runtime context. This
//! helper calls authenticated local AionCore routes; it never receives the GEA
//! login token or delegation token.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

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

use super::gea_result_projection::{BusinessDataAction, project_business_data_result};

const ENV_BASE_URL: &str = "AIONUI_BASE_URL";
const ENV_CONVERSATION_ID: &str = "AIONUI_CONVERSATION_ID";
const ENV_USER_ID: &str = "AIONUI_USER_ID";
const ENV_RUNTIME_TOKEN: &str = "AIONUI_RUNTIME_TOKEN";
const ENV_AGENT_CODE: &str = "AIONUI_GEA_AGENT_CODE";
const DEFAULT_AGENT_CODE: &str = "sales_forecast";
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BACKEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(125);
const SESSION_START_MAX_RETRIES: usize = 5;
const TOOL_CALL_MAX_RETRIES: usize = 5;
const SESSION_RETRY_MAX_DELAY: Duration = Duration::from_secs(8);
const BUSINESS_DATA_ARTIFACT_MAX_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOOL_NAME_LENGTH: usize = 64;
const GEA_IDENTITY_BOOTSTRAP_TOOL: &str = "gateway.session.currentUser.resolve";
const BUSINESS_DATA_TOOL_NAME: &str = "query_business_data";
const GEA_MCP_INSTRUCTIONS: &str = "GEA enterprise tools scoped to the current AionUi conversation. Use these tools as the only route for GEA-governed data. Never discover or call direct upstream MCP endpoints, and never reuse credentials from client configuration. If retryable GEA errors persist, report the gateway error instead of bypassing GEA.";
const GEA_RETRYABLE_RECOVERY_HINT: &str = "Retry the same GEA tool after retryAfterMs or a short delay. Do not discover or call direct upstream MCP endpoints; report the GEA error if retries remain unsuccessful.";

pub async fn run_gea_stdio() -> ExitCode {
    let env = match GeaStdioEnv::from_env() {
        Ok(env) => env,
        Err(code) => {
            eprintln!("GEA_MCP_ENV_INVALID field={code}");
            return ExitCode::FAILURE;
        }
    };
    let client = match reqwest::Client::builder()
        .connect_timeout(BACKEND_CONNECT_TIMEOUT)
        .timeout(BACKEND_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            eprintln!("GEA_MCP_CLIENT_INIT_FAILED");
            return ExitCode::FAILURE;
        }
    };
    let server = GeaStdioServer {
        client,
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

#[derive(Deserialize)]
struct StandardApiErrorBody {
    error: String,
    code: String,
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

struct BusinessDataArtifact {
    url: reqwest::Url,
    size: usize,
    sha256: String,
}

fn uses_legacy_business_data_arguments(tool: &ToolInfo) -> bool {
    tool.name == BUSINESS_DATA_TOOL_NAME
        && tool
            .input_schema
            .get("properties")
            .and_then(|properties| properties.get("queries"))
            .and_then(|queries| queries.get("type"))
            .and_then(Value::as_str)
            == Some("string")
}

fn exposed_input_schema(tool: &ToolInfo) -> Value {
    if tool.name != BUSINESS_DATA_TOOL_NAME {
        return tool.input_schema.clone();
    }
    business_data_input_schema()
}

fn business_data_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["inspect", "query"],
                "description": "inspect reads the semantic-model catalog; query executes one to eight named Cube queries."
            },
            "queries": {
                "type": "array",
                "maxItems": 8,
                "description": "Use an empty array for inspect. For query, provide one to eight named query objects.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "query"],
                    "properties": {
                        "name": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Stable name for this query in the returned result."
                        },
                        "query": {
                            "type": "object",
                            "additionalProperties": true,
                            "description": "Cube JSON Query. Put all query fields inside this object.",
                            "properties": {
                                "measures": { "type": "array", "items": { "type": "string" } },
                                "dimensions": { "type": "array", "items": { "type": "string" } },
                                "filters": { "type": "array", "items": { "type": "object" } },
                                "timeDimensions": { "type": "array", "items": { "type": "object" } },
                                "segments": { "type": "array", "items": { "type": "string" } },
                                "limit": { "type": "integer", "minimum": 0 },
                                "order": {
                                    "oneOf": [
                                        { "type": "object", "additionalProperties": { "type": "string", "enum": ["asc", "desc"] } },
                                        { "type": "array", "items": { "type": "object" } }
                                    ]
                                }
                            }
                        }
                    }
                }
            },
            "model": {
                "type": "string",
                "minLength": 1,
                "description": "For inspect, omit this field to list the complete model catalog, then set it to one exact model name to retrieve that model schema. Ignored for query."
            }
        },
        "required": ["action", "queries"]
    })
}

fn gateway_arguments(tool: &ToolInfo, mut arguments: Value) -> Value {
    if tool.name != BUSINESS_DATA_TOOL_NAME {
        return arguments;
    }
    if let Some(arguments) = arguments.as_object_mut() {
        arguments.remove("model");
    }
    normalize_business_data_queries(&mut arguments);
    if !uses_legacy_business_data_arguments(tool) {
        return arguments;
    }
    let Some(queries) = arguments.get("queries").filter(|queries| queries.is_array()) else {
        return arguments;
    };
    let Ok(serialized) = serde_json::to_string(queries) else {
        return arguments;
    };
    if let Some(arguments) = arguments.as_object_mut() {
        arguments.insert("queries".to_owned(), Value::String(serialized));
    }
    arguments
}

fn normalize_business_data_queries(arguments: &mut Value) {
    let Some(queries) = arguments.get_mut("queries").and_then(Value::as_array_mut) else {
        return;
    };
    let mut used_names = queries
        .iter()
        .filter_map(|item| item.get("query").and_then(|_| item.get("name")))
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<HashSet<_>>();

    for (index, item) in queries.iter_mut().enumerate() {
        let is_named = item.get("query").is_some();
        let has_name = item
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty());
        if is_named && has_name {
            continue;
        }
        let name = unique_query_name(index, &used_names);
        used_names.insert(name.clone());
        if is_named {
            if let Some(object) = item.as_object_mut() {
                object.insert("name".to_owned(), Value::String(name));
            }
        } else if item.is_object() {
            let query = std::mem::take(item);
            *item = json!({ "name": name, "query": query });
        }
    }
}

fn unique_query_name(index: usize, used_names: &HashSet<String>) -> String {
    let mut suffix = index + 1;
    loop {
        let candidate = format!("query_{suffix}");
        if !used_names.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn adapt_business_data_result(tool: &ToolInfo, arguments: &Value, result: Value) -> Result<Value, McpError> {
    if tool.name != BUSINESS_DATA_TOOL_NAME {
        return Ok(result);
    }
    let action = match arguments.get("action").and_then(Value::as_str) {
        Some("inspect") => BusinessDataAction::Inspect {
            model: arguments
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty()),
        },
        Some("query") => BusinessDataAction::Query,
        _ => BusinessDataAction::Other,
    };
    project_business_data_result(action, result).map_err(|error| {
        if error.code == "GEA_MCP_SEMANTIC_MODEL_NOT_FOUND" {
            McpError::invalid_params(error.code, error.details)
        } else {
            McpError::internal_error(error.code, error.details)
        }
    })
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
        for attempt in 1..=(SESSION_START_MAX_RETRIES + 1) {
            match self
                .request::<Value>(
                    reqwest::Method::POST,
                    &path,
                    Some(json!({
                        "consumerCode": self.env.agent_code
                    })),
                )
                .await
            {
                Ok(_) => break,
                Err(error) => {
                    let Some(delay) = session_retry_delay(&error, attempt) else {
                        return Err(error);
                    };
                    tracing::warn!(
                        event = "gea_session_start_retry",
                        conversation_id = %self.env.conversation_id,
                        retry_attempt = attempt,
                        max_retries = SESSION_START_MAX_RETRIES,
                        delay_ms = delay.as_millis(),
                        code = error_field(&error, "code").unwrap_or("unknown"),
                        category = error_field(&error, "category").unwrap_or("unknown"),
                        "retrying GEA session startup after retryable backend error"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
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
    let error = serde_json::from_slice::<ApiErrorBody>(body).or_else(|_| {
        serde_json::from_slice::<StandardApiErrorBody>(body).map(|standard| {
            let details = standard.details.unwrap_or(Value::Null);
            ApiErrorBody {
                code: standard.code,
                message: standard.error,
                category: details
                    .get("category")
                    .and_then(Value::as_str)
                    .unwrap_or("INTERNAL")
                    .to_owned(),
                retryable: details.get("retryable").and_then(Value::as_bool).unwrap_or(false),
                retry_after_ms: details.get("retryAfterMs").and_then(Value::as_u64),
                request_id: details.get("requestId").and_then(Value::as_str).map(str::to_owned),
                trace_id: details.get("traceId").and_then(Value::as_str).map(str::to_owned),
                audit_id: details.get("auditId").and_then(Value::as_str).map(str::to_owned),
                details: details.get("upstream").cloned(),
            }
        })
    });
    let Ok(error) = error else {
        return McpError::internal_error(format!("GEA_MCP_BACKEND_HTTP_{}", status.as_u16()), None);
    };
    let message = format!(
        "{}: {} [category={} retryable={}]",
        error.code, error.message, error.category, error.retryable
    );
    let retryable = error.retryable;
    let mut data = serde_json::to_value(error).ok();
    if retryable && let Some(data) = data.as_mut().and_then(Value::as_object_mut) {
        data.insert(
            "recoveryHint".to_owned(),
            Value::String(GEA_RETRYABLE_RECOVERY_HINT.to_owned()),
        );
    }
    McpError::internal_error(message, data)
}

fn should_reset_session_after_error(error: &McpError) -> bool {
    matches!(error_field(error, "category"), Some("AUTHENTICATION" | "SESSION"))
}

fn error_field<'a>(error: &'a McpError, field: &str) -> Option<&'a str> {
    error.data.as_ref()?.get(field)?.as_str()
}

fn session_retry_delay(error: &McpError, failed_attempt: usize) -> Option<Duration> {
    if failed_attempt > SESSION_START_MAX_RETRIES
        || error.data.as_ref()?.get("retryable").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let retry_after = error
        .data
        .as_ref()
        .and_then(|data| data.get("retryAfterMs"))
        .and_then(Value::as_u64)
        .map(Duration::from_millis);
    let exponential = Duration::from_millis(250 * (1_u64 << (failed_attempt - 1)));
    Some(retry_after.unwrap_or(exponential).min(SESSION_RETRY_MAX_DELAY))
}

fn tool_call_retry_delay(
    tool: &ToolInfo,
    arguments: &Value,
    error: &McpError,
    failed_attempt: usize,
) -> Option<Duration> {
    if failed_attempt > TOOL_CALL_MAX_RETRIES
        || tool.name != BUSINESS_DATA_TOOL_NAME
        || !matches!(
            arguments.get("action").and_then(Value::as_str),
            Some("inspect" | "query")
        )
    {
        return None;
    }
    let category = error_field(error, "category");
    if matches!(category, Some("AUTHENTICATION" | "AUTHORIZATION" | "VALIDATION")) {
        return None;
    }
    let gateway_retryable = error
        .data
        .as_ref()
        .and_then(|data| data.get("retryable"))
        .and_then(Value::as_bool)
        == Some(true);
    let safe_read_retry = matches!(category, Some("UPSTREAM" | "RATE_LIMIT" | "SESSION"));
    if !gateway_retryable && !safe_read_retry {
        return None;
    }
    let retry_after = error
        .data
        .as_ref()
        .and_then(|data| data.get("retryAfterMs"))
        .and_then(Value::as_u64)
        .map(Duration::from_millis);
    let exponential = Duration::from_millis(250 * (1_u64 << (failed_attempt - 1)));
    Some(retry_after.unwrap_or(exponential).min(SESSION_RETRY_MAX_DELAY))
}

fn retryable_artifact_error(code: &'static str) -> McpError {
    McpError::internal_error(
        code,
        Some(json!({
            "code": code,
            "category": "UPSTREAM",
            "retryable": true
        })),
    )
}

impl ServerHandler for GeaStdioServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(GEA_MCP_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .load_tools()
            .await?
            .into_iter()
            .map(|(exposed_name, tool)| {
                let input_schema = exposed_input_schema(&tool)
                    .as_object()
                    .cloned()
                    .unwrap_or_else(|| Map::from_iter([("type".to_owned(), Value::String("object".to_owned()))]));
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
        let gateway_arguments = gateway_arguments(&tool, arguments.clone());
        let mut failed_attempt = 0;
        let hydrated_result = loop {
            let attempt_result = match self
                .request::<ToolCallResponse>(
                    reqwest::Method::POST,
                    &path,
                    Some(json!({ "arguments": gateway_arguments.clone() })),
                )
                .await
            {
                Ok(response) => {
                    hydrate_business_data_artifact(&self.client, &self.env.conversation_id, &tool, response.result)
                        .await
                }
                Err(error) => Err(error),
            };
            match attempt_result {
                Ok(result) => break result,
                Err(error) => {
                    failed_attempt += 1;
                    if should_reset_session_after_error(&error) {
                        *self.session_ready.lock().await = false;
                    }
                    let Some(delay) = tool_call_retry_delay(&tool, &arguments, &error, failed_attempt) else {
                        return Err(error);
                    };
                    tracing::warn!(
                        event = "gea_tool_call_retry",
                        conversation_id = %self.env.conversation_id,
                        tool_name = %tool.name,
                        retry_attempt = failed_attempt,
                        max_retries = TOOL_CALL_MAX_RETRIES,
                        delay_ms = delay.as_millis(),
                        code = error_field(&error, "code").unwrap_or("unknown"),
                        category = error_field(&error, "category").unwrap_or("unknown"),
                        "retrying read-only GEA tool after gateway failure"
                    );
                    tokio::time::sleep(delay).await;
                    self.ensure_session().await?;
                }
            }
        };
        let result_value = adapt_business_data_result(&tool, &arguments, hydrated_result)?;
        let text = match &result_value {
            Value::String(value) => value.clone(),
            value => serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()),
        };
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        if result_value.is_object() {
            result.structured_content = Some(result_value);
        }
        Ok(result)
    }
}

async fn hydrate_business_data_artifact(
    client: &reqwest::Client,
    conversation_id: &str,
    tool: &ToolInfo,
    result: Value,
) -> Result<Value, McpError> {
    if tool.name != BUSINESS_DATA_TOOL_NAME {
        return Ok(result);
    }
    let Some(artifact) = parse_business_data_artifact(&result).map_err(|code| McpError::internal_error(code, None))?
    else {
        return Ok(result);
    };
    let mut response = client
        .get(artifact.url.clone())
        .send()
        .await
        .map_err(|_| retryable_artifact_error("GEA_MCP_ARTIFACT_DOWNLOAD_FAILED"))?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > BUSINESS_DATA_ARTIFACT_MAX_BYTES as u64)
    {
        return Err(retryable_artifact_error("GEA_MCP_ARTIFACT_DOWNLOAD_FAILED"));
    }
    let mut bytes = Vec::with_capacity(artifact.size);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| retryable_artifact_error("GEA_MCP_ARTIFACT_DOWNLOAD_FAILED"))?
    {
        if bytes.len().saturating_add(chunk.len()) > BUSINESS_DATA_ARTIFACT_MAX_BYTES {
            return Err(McpError::internal_error("GEA_MCP_ARTIFACT_TOO_LARGE", None));
        }
        bytes.extend_from_slice(&chunk);
    }
    validate_business_data_artifact(&artifact, &bytes).map_err(|code| McpError::internal_error(code, None))?;
    let hydrated =
        serde_json::from_slice(&bytes).map_err(|_| McpError::internal_error("GEA_MCP_ARTIFACT_INVALID", None))?;
    tracing::info!(
        conversation_id,
        artifact_bytes = bytes.len(),
        "GEA business data artifact hydrated"
    );
    Ok(hydrated)
}

fn parse_business_data_artifact(result: &Value) -> Result<Option<BusinessDataArtifact>, &'static str> {
    if result.get("type").and_then(Value::as_str) != Some("resource_link") {
        return Ok(None);
    }
    let uri = result
        .get("uri")
        .and_then(Value::as_str)
        .ok_or("GEA_MCP_ARTIFACT_INVALID")?;
    if !uri.starts_with("data-artifact://")
        || result.get("mimeType").and_then(Value::as_str) != Some("application/json")
    {
        return Err("GEA_MCP_ARTIFACT_INVALID");
    }
    let size = result
        .get("size")
        .and_then(Value::as_u64)
        .and_then(|size| usize::try_from(size).ok())
        .filter(|size| *size <= BUSINESS_DATA_ARTIFACT_MAX_BYTES)
        .ok_or("GEA_MCP_ARTIFACT_TOO_LARGE")?;
    let metadata = result
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or("GEA_MCP_ARTIFACT_INVALID")?;
    let raw_url = metadata
        .get("oss_url")
        .or_else(|| metadata.get("ossUrl"))
        .and_then(Value::as_str)
        .ok_or("GEA_MCP_ARTIFACT_INVALID")?;
    let url = reqwest::Url::parse(raw_url).map_err(|_| "GEA_MCP_ARTIFACT_INVALID")?;
    let host = url.host_str().ok_or("GEA_MCP_ARTIFACT_INVALID")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !host.ends_with(".aliyuncs.com")
    {
        return Err("GEA_MCP_ARTIFACT_URL_DENIED");
    }
    let sha256 = metadata
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("GEA_MCP_ARTIFACT_INVALID")?
        .to_ascii_lowercase();
    Ok(Some(BusinessDataArtifact { url, size, sha256 }))
}

fn validate_business_data_artifact(artifact: &BusinessDataArtifact, bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() != artifact.size {
        return Err("GEA_MCP_ARTIFACT_INTEGRITY_FAILED");
    }
    let digest = format!("{:x}", Sha256::digest(bytes));
    if digest != artifact.sha256 {
        return Err("GEA_MCP_ARTIFACT_INTEGRITY_FAILED");
    }
    Ok(())
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tokio::sync::{Mutex, RwLock};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        GEA_MCP_INSTRUCTIONS, GEA_RETRYABLE_RECOVERY_HINT, GeaStdioEnv, GeaStdioServer, SESSION_START_MAX_RETRIES,
        TOOL_CALL_MAX_RETRIES, ToolInfo, backend_mcp_error, compatible_tool_name, exposed_input_schema,
        gateway_arguments, parse_business_data_artifact, session_retry_delay, should_expose_tool_to_agent,
        should_reset_session_after_error, tool_call_retry_delay, validate_business_data_artifact,
    };

    fn legacy_business_data_tool() -> ToolInfo {
        ToolInfo {
            name: "query_business_data".to_owned(),
            source_code: "cube".to_owned(),
            description: "Query business data".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string" },
                    "queries": { "type": "string" }
                },
                "required": ["action", "queries"]
            }),
        }
    }

    fn current_business_data_tool() -> ToolInfo {
        ToolInfo {
            name: "query_business_data".to_owned(),
            source_code: "cube".to_owned(),
            description: "Query business data".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string" },
                    "queries": { "type": "array", "items": { "type": "object" } }
                },
                "required": ["action", "queries"]
            }),
        }
    }

    #[test]
    fn legacy_business_data_tool_exposes_named_query_schema() {
        let schema = exposed_input_schema(&legacy_business_data_tool());

        assert_eq!(schema["properties"]["action"]["enum"], json!(["inspect", "query"]));
        assert_eq!(schema["properties"]["queries"]["type"], "array");
        assert_eq!(schema["properties"]["queries"]["maxItems"], 8);
        assert_eq!(schema["properties"]["model"]["type"], "string");
        assert_eq!(
            schema["properties"]["queries"]["items"]["required"],
            json!(["name", "query"])
        );
    }

    #[test]
    fn legacy_business_data_tool_serializes_named_queries_for_gea() {
        let queries = json!([{
            "name": "sales_forecast_probe",
            "query": {
                "measures": ["agents_sales_forecast_detail.row_count"],
                "limit": 1
            }
        }]);

        let adapted = gateway_arguments(
            &legacy_business_data_tool(),
            json!({ "action": "query", "queries": queries, "model": "ignored-for-query" }),
        );

        assert_eq!(adapted["action"], "query");
        assert_eq!(
            adapted["queries"],
            serde_json::to_string(&queries).expect("serialize named queries")
        );
        assert!(adapted.get("model").is_none());
    }

    #[test]
    fn current_business_data_tool_exposes_named_query_schema() {
        let schema = exposed_input_schema(&current_business_data_tool());

        assert_eq!(
            schema["properties"]["queries"]["items"]["required"],
            json!(["name", "query"])
        );
    }

    #[test]
    fn current_business_data_tool_wraps_flat_queries_for_gea() {
        let adapted = gateway_arguments(
            &current_business_data_tool(),
            json!({
                "action": "query",
                "queries": [
                    {
                        "measures": ["agents_sales_forecast_detail.row_count"],
                        "limit": 1
                    },
                    {
                        "name": "already_named",
                        "query": { "measures": ["agents_sales_forecast_detail.dealer_count"] }
                    }
                ]
            }),
        );

        assert_eq!(adapted["queries"][0]["name"], "query_1");
        assert_eq!(
            adapted["queries"][0]["query"]["measures"],
            json!(["agents_sales_forecast_detail.row_count"])
        );
        assert_eq!(adapted["queries"][1]["name"], "already_named");
    }

    #[test]
    fn generated_query_names_do_not_collide_with_explicit_names() {
        let adapted = gateway_arguments(
            &current_business_data_tool(),
            json!({
                "action": "query",
                "queries": [
                    { "name": "query_1", "query": { "limit": 1 } },
                    { "measures": ["agents_sales_forecast_detail.row_count"] }
                ]
            }),
        );

        assert_eq!(adapted["queries"][0]["name"], "query_1");
        assert_eq!(adapted["queries"][1]["name"], "query_2");
    }

    #[test]
    fn business_data_artifact_requires_trusted_https_and_matching_integrity() {
        let bytes = br#"{"status":"completed","datasets":[]}"#;
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let result = json!({
            "type": "resource_link",
            "uri": "data-artifact://artifact-1",
            "mimeType": "application/json",
            "size": bytes.len(),
            "_meta": {
                "oss_url": "https://example.oss-cn-beijing.aliyuncs.com/artifact.json?signature=redacted",
                "sha256": sha256
            }
        });

        let artifact = parse_business_data_artifact(&result)
            .expect("valid artifact metadata")
            .expect("resource link artifact");
        assert_eq!(artifact.size, bytes.len());
        assert!(validate_business_data_artifact(&artifact, bytes).is_ok());
        assert_eq!(
            validate_business_data_artifact(&artifact, b"{}"),
            Err("GEA_MCP_ARTIFACT_INTEGRITY_FAILED")
        );

        let mut denied = result;
        denied["_meta"]["oss_url"] = json!("http://127.0.0.1/private");
        assert!(matches!(
            parse_business_data_artifact(&denied),
            Err("GEA_MCP_ARTIFACT_URL_DENIED")
        ));
    }

    #[test]
    fn read_only_business_query_retries_upstream_resource_failures_at_most_five_times() {
        let error = backend_mcp_error(
            reqwest::StatusCode::BAD_GATEWAY,
            r#"{
                "code":"MCP_RESOURCE_INCOMPLETE",
                "message":"query_business_data Resource 分页读取失败",
                "category":"UPSTREAM",
                "retryable":false
            }"#
            .as_bytes(),
        );
        let arguments = json!({ "action": "query", "queries": [] });

        assert_eq!(
            tool_call_retry_delay(&current_business_data_tool(), &arguments, &error, 1),
            Some(Duration::from_millis(250))
        );
        assert!(
            tool_call_retry_delay(&current_business_data_tool(), &arguments, &error, TOOL_CALL_MAX_RETRIES).is_some()
        );
        assert_eq!(
            tool_call_retry_delay(
                &current_business_data_tool(),
                &arguments,
                &error,
                TOOL_CALL_MAX_RETRIES + 1
            ),
            None
        );

        let authorization_error = backend_mcp_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{
                "code":"AI_GATEWAY_DATA_PERMISSION_DENIED",
                "message":"forbidden",
                "category":"AUTHORIZATION",
                "retryable":false
            }"#
            .as_bytes(),
        );
        assert_eq!(
            tool_call_retry_delay(&current_business_data_tool(), &arguments, &authorization_error, 1),
            None
        );
    }

    #[test]
    fn agent_projection_excludes_gateway_identity_bootstrap_tool() {
        assert!(!should_expose_tool_to_agent("gateway.session.currentUser.resolve"));
        assert!(should_expose_tool_to_agent("query_business_data"));
    }

    #[test]
    fn gea_server_instructions_forbid_direct_upstream_fallback() {
        let server = GeaStdioServer {
            client: reqwest::Client::new(),
            env: GeaStdioEnv {
                base_url: "http://127.0.0.1".to_owned(),
                conversation_id: "conversation-1".to_owned(),
                user_id: "user-1".to_owned(),
                runtime_token: "runtime-token".to_owned(),
                agent_code: "sales_forecast".to_owned(),
            },
            session_ready: Arc::new(Mutex::new(false)),
            tools: Arc::new(RwLock::new(HashMap::new())),
        };

        let instructions = rmcp::ServerHandler::get_info(&server)
            .instructions
            .expect("GEA MCP instructions");

        assert_eq!(instructions, GEA_MCP_INSTRUCTIONS);
        assert!(instructions.contains("Never discover or call direct upstream MCP endpoints"));
        assert!(instructions.contains("instead of bypassing GEA"));
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

    #[tokio::test]
    async fn upstream_failure_keeps_the_ready_session() {
        let upstream = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/gea/conversations/conversation-1/tools"))
            .respond_with(ResponseTemplate::new(502).set_body_json(json!({
                "code": "GEA_INVALID_RESPONSE",
                "message": "GEA 返回无效响应",
                "category": "UPSTREAM",
                "retryable": true
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
            Ok(_) => panic!("upstream failure must surface"),
            Err(error) => error,
        };

        assert_eq!(
            error.data.as_ref().and_then(|data| data["category"].as_str()),
            Some("UPSTREAM")
        );
        assert!(*server.session_ready.lock().await);
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_session_failures_are_retried_within_the_same_startup() {
        let upstream = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let responder_attempts = Arc::clone(&attempts);
        Mock::given(method("POST"))
            .and(path("/api/gea/conversations/conversation-1/session"))
            .respond_with(move |_request: &wiremock::Request| {
                if responder_attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                    ResponseTemplate::new(502).set_body_json(json!({
                        "code": "GEA_SESSION_UPSTREAM_FAILED",
                        "message": "GEA 会话创建失败",
                        "category": "UPSTREAM",
                        "retryable": true
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({ "data": {} }))
                }
            })
            .expect(3)
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

        server.ensure_session().await.expect("retryable session should recover");

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
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
    fn backend_error_reads_the_standard_api_error_envelope() {
        let error = backend_mcp_error(
            reqwest::StatusCode::BAD_GATEWAY,
            r#"{
                "success":false,
                "error":"GEA 会话创建失败",
                "code":"GEA_SESSION_UPSTREAM_FAILED",
                "details":{
                    "category":"UPSTREAM",
                    "retryable":true,
                    "requestId":"request-2",
                    "traceId":"trace-2"
                }
            }"#
            .as_bytes(),
        );
        let value = serde_json::to_value(&error).expect("serialize MCP error");

        assert_eq!(
            value["message"],
            "GEA_SESSION_UPSTREAM_FAILED: GEA 会话创建失败 [category=UPSTREAM retryable=true]"
        );
        assert_eq!(value["data"]["code"], "GEA_SESSION_UPSTREAM_FAILED");
        assert_eq!(value["data"]["requestId"], "request-2");
        assert_eq!(value["data"]["traceId"], "trace-2");
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
        assert_eq!(value["data"]["recoveryHint"], GEA_RETRYABLE_RECOVERY_HINT);
        assert_eq!(session_retry_delay(&error, 1), Some(Duration::from_secs(2)));
        assert_eq!(
            session_retry_delay(&error, SESSION_START_MAX_RETRIES),
            Some(Duration::from_secs(2))
        );
        assert_eq!(session_retry_delay(&error, SESSION_START_MAX_RETRIES + 1), None);
        assert!(!should_reset_session_after_error(&error));
    }

    #[test]
    fn only_authentication_and_session_failures_require_session_recovery() {
        for category in ["AUTHENTICATION", "SESSION"] {
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

        for category in ["AUTHORIZATION", "RATE_LIMIT", "UPSTREAM", "INTERNAL"] {
            let error = backend_mcp_error(
                reqwest::StatusCode::BAD_GATEWAY,
                json!({
                    "code": "GEA_SESSION_STILL_VALID",
                    "message": "session remains valid",
                    "category": category,
                    "retryable": true
                })
                .to_string()
                .as_bytes(),
            );

            assert!(!should_reset_session_after_error(&error), "category={category}");
        }

        let unstructured = rmcp::ErrorData::internal_error("GEA_MCP_BACKEND_UNAVAILABLE", None);
        assert!(!should_reset_session_after_error(&unstructured));
    }
}
