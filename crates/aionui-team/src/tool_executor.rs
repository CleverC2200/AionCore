use std::sync::Weak;
use std::time::Instant;

use aionui_api_types::{
    CreateTeamWorkTaskRequest, TeamToolCall, TeamToolDescriptor, TeamToolErrorCode, TeamToolErrorPayload, TeamToolName,
    TeamToolRole, TeamToolTransport, TeamWorkActor, TeamWorkActorKind, TeamWorkCommand, TeamWorkCommandEnvelope,
    TeamWorkErrorCode,
};
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::TeamError;
use crate::mcp::server::ToolCallError;
use crate::scheduler::TeammateManager;
use crate::service::TeamSessionService;
use crate::types::TeammateRole;

#[derive(Debug, Clone)]
pub struct TeamToolContext {
    pub team_id: String,
    pub caller_slot_id: String,
    pub caller_role: TeammateRole,
    pub user_id: Option<String>,
    pub conversation_id: Option<String>,
    pub transport: TeamToolTransport,
}

pub struct TeamToolExecutor<'a> {
    scheduler: &'a TeammateManager,
    service: &'a Weak<TeamSessionService>,
}

impl<'a> TeamToolExecutor<'a> {
    pub fn new(scheduler: &'a TeammateManager, service: &'a Weak<TeamSessionService>) -> Self {
        Self { scheduler, service }
    }

    pub fn list_tools(&self, context: &TeamToolContext) -> Vec<TeamToolDescriptor> {
        aionui_api_types::team_tool_descriptors_for_role(team_tool_role(context.caller_role))
    }

    pub async fn execute(&self, context: &TeamToolContext, call: TeamToolCall) -> Result<Value, TeamToolErrorPayload> {
        let started = Instant::now();
        let tool = call.tool.as_str();
        let result = if matches!(
            call.tool,
            TeamToolName::TeamWorkCreate | TeamToolName::TeamWorkList | TeamToolName::TeamWorkCommand
        ) {
            self.execute_team_work(context, call).await
        } else {
            crate::mcp::server::dispatch_tool(
                tool,
                &call.arguments,
                self.scheduler,
                self.service,
                &context.team_id,
                &context.caller_slot_id,
                context.caller_role,
            )
            .await
            .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text)))
            .map_err(map_tool_error)
        };

        let duration_ms = started.elapsed().as_millis();
        match &result {
            Ok(_) => info!(
                transport = ?context.transport,
                team_id = %context.team_id,
                caller_slot_id = %context.caller_slot_id,
                conversation_id = context.conversation_id.as_deref().unwrap_or(""),
                tool,
                result = "success",
                duration_ms,
                "Team tool call completed"
            ),
            Err(error) => warn!(
                transport = ?context.transport,
                team_id = %context.team_id,
                caller_slot_id = %context.caller_slot_id,
                conversation_id = context.conversation_id.as_deref().unwrap_or(""),
                tool,
                error_code = ?error.code,
                classification = error.details.as_ref().and_then(|details| details.get("classification")).and_then(|value| value.as_str()).unwrap_or("business_error"),
                duration_ms,
                "Team tool call failed"
            ),
        }
        result
    }

    async fn execute_team_work(
        &self,
        context: &TeamToolContext,
        call: TeamToolCall,
    ) -> Result<Value, TeamToolErrorPayload> {
        let user_id = context.user_id.as_deref().ok_or_else(|| {
            TeamToolErrorPayload::new(
                TeamToolErrorCode::RuntimeAuthFailed,
                "Team Work requires an authenticated user",
            )
        })?;
        let service = self
            .service
            .upgrade()
            .and_then(|service| service.team_work_service())
            .ok_or_else(|| {
                TeamToolErrorPayload::new(
                    TeamToolErrorCode::TransportUnavailable,
                    "Team Work service not available",
                )
            })?;
        match call.tool {
            TeamToolName::TeamWorkCreate => {
                let request: CreateTeamWorkTaskRequest = serde_json::from_value(call.arguments).map_err(|_| {
                    TeamToolErrorPayload::new(
                        TeamToolErrorCode::SchemaValidationFailed,
                        "Invalid team_work_create arguments",
                    )
                })?;
                serde_json::to_value(
                    service
                        .create_task(user_id, &context.team_id, request)
                        .await
                        .map_err(map_work_error)?,
                )
                .map_err(|_| {
                    TeamToolErrorPayload::new(
                        TeamToolErrorCode::RuntimeContextMissing,
                        "Failed to encode Team Work task",
                    )
                })
            }
            TeamToolName::TeamWorkList => {
                if call
                    .arguments
                    .as_object()
                    .is_some_and(|arguments| !arguments.is_empty())
                {
                    return Err(TeamToolErrorPayload::new(
                        TeamToolErrorCode::SchemaValidationFailed,
                        "team_work_list does not accept arguments",
                    ));
                }
                serde_json::to_value(
                    service
                        .snapshot(user_id, &context.team_id)
                        .await
                        .map_err(map_work_error)?,
                )
                .map_err(|_| {
                    TeamToolErrorPayload::new(
                        TeamToolErrorCode::RuntimeContextMissing,
                        "Failed to encode Team Work snapshot",
                    )
                })
            }
            TeamToolName::TeamWorkCommand => {
                #[derive(Deserialize)]
                struct CommandArgs {
                    task_id: String,
                    expected_version: u64,
                    idempotency_key: String,
                    command: TeamWorkCommand,
                }
                let arguments: CommandArgs = serde_json::from_value(call.arguments).map_err(|_| {
                    TeamToolErrorPayload::new(
                        TeamToolErrorCode::SchemaValidationFailed,
                        "Invalid team_work_command arguments",
                    )
                })?;
                let receipt = service
                    .apply_command(
                        user_id,
                        &context.team_id,
                        &arguments.task_id,
                        TeamWorkCommandEnvelope {
                            expected_version: arguments.expected_version,
                            idempotency_key: arguments.idempotency_key,
                            actor: TeamWorkActor {
                                kind: TeamWorkActorKind::Agent,
                                id: context.caller_slot_id.clone(),
                            },
                            command: arguments.command,
                        },
                    )
                    .await
                    .map_err(map_work_error)?;
                serde_json::to_value(receipt).map_err(|_| {
                    TeamToolErrorPayload::new(
                        TeamToolErrorCode::RuntimeContextMissing,
                        "Failed to encode Team Work receipt",
                    )
                })
            }
            _ => unreachable!("Team Work dispatch is guarded by the caller"),
        }
    }
}

fn map_work_error(error: TeamError) -> TeamToolErrorPayload {
    let code = match &error {
        TeamError::WorkState(state) => match state.code {
            TeamWorkErrorCode::VersionConflict => TeamToolErrorCode::VersionConflict,
            TeamWorkErrorCode::LeaseConflict | TeamWorkErrorCode::LeaseExpired => TeamToolErrorCode::LeaseConflict,
            TeamWorkErrorCode::InvalidTransition => TeamToolErrorCode::InvalidTransition,
            TeamWorkErrorCode::IdempotencyConflict => TeamToolErrorCode::IdempotencyConflict,
            TeamWorkErrorCode::DependencyBlocked => TeamToolErrorCode::DependencyBlocked,
            TeamWorkErrorCode::RetryLimitReached => TeamToolErrorCode::RetryLimitReached,
            TeamWorkErrorCode::ActorForbidden => TeamToolErrorCode::PermissionDenied,
        },
        TeamError::TeamNotFound(_) | TeamError::TaskNotFound(_) => TeamToolErrorCode::TeamNotFound,
        TeamError::Forbidden(_) | TeamError::LeaderOnly(_) => TeamToolErrorCode::PermissionDenied,
        TeamError::InvalidRequest(_) | TeamError::BlockedTaskNotFound(_) => TeamToolErrorCode::SchemaValidationFailed,
        _ => TeamToolErrorCode::RuntimeContextMissing,
    };
    TeamToolErrorPayload::new(code, error.to_string())
}

pub fn team_tool_call_from_name(tool_name: &str, arguments: Value) -> Result<TeamToolCall, TeamToolErrorPayload> {
    let tool = TeamToolName::parse(tool_name).ok_or_else(|| {
        TeamToolErrorPayload::new(TeamToolErrorCode::UnknownTool, format!("Unknown tool: {tool_name}"))
    })?;
    Ok(TeamToolCall { tool, arguments })
}

fn team_tool_role(role: TeammateRole) -> TeamToolRole {
    match role {
        TeammateRole::Lead => TeamToolRole::Lead,
        TeammateRole::Teammate => TeamToolRole::Teammate,
    }
}

fn map_tool_error(error: ToolCallError) -> TeamToolErrorPayload {
    let code = if error.message.starts_with("Unknown tool:") {
        TeamToolErrorCode::UnknownTool
    } else if error.message.starts_with("Only Lead") {
        TeamToolErrorCode::PermissionDenied
    } else if error.message.starts_with("Invalid params")
        || error.message.starts_with("Missing required field")
        || error.message.contains("does not accept arguments")
        || error.message.contains("is no longer accepted")
    {
        TeamToolErrorCode::SchemaValidationFailed
    } else if error.message.contains("Invalid agent target") {
        TeamToolErrorCode::AgentNotFound
    } else if error.message.contains("No active session") {
        TeamToolErrorCode::TeamNotFound
    } else if error.message.contains("Team service not available") {
        TeamToolErrorCode::TransportUnavailable
    } else {
        TeamToolErrorCode::RuntimeContextMissing
    };

    let mut payload = TeamToolErrorPayload::new(code, error.message);
    if let Some(details) = error.details {
        payload = payload.with_details(details);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_tool_maps_to_shared_error_payload() {
        let err = team_tool_call_from_name("missing_tool", json!({})).unwrap_err();
        assert_eq!(err.code, TeamToolErrorCode::UnknownTool);
        assert!(err.message.contains("Unknown tool"));
    }
}
