use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkTaskStatus {
    Backlog,
    Ready,
    Claimed,
    Running,
    NeedsInput,
    NeedsApproval,
    Blocked,
    InReview,
    Done,
    Failed,
    Cancelled,
}

impl TeamWorkTaskStatus {
    pub fn is_attention(self) -> bool {
        matches!(
            self,
            Self::NeedsInput | Self::NeedsApproval | Self::InReview | Self::Failed
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkRunStatus {
    Queued,
    Starting,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Stale,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkPriority {
    Urgent,
    High,
    #[default]
    Normal,
    Low,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkNextActionOwner {
    Agent,
    Human,
    Reviewer,
    System,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkApprovalState {
    #[default]
    None,
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkQueueReason {
    TeamCapacity,
    AgentCapacity,
    ProfileCapacity,
    WorkspaceLocked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkActorKind {
    Agent,
    Human,
    Reviewer,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamWorkActor {
    pub kind: TeamWorkActorKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamWorkLease {
    pub holder: String,
    pub expires_at: TimestampMs,
    pub heartbeat_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamWorkVerificationCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub result: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TeamWorkVerificationReceipt {
    #[serde(default)]
    pub checks: Vec<TeamWorkVerificationCheck>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub remaining_risks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkRunUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamWorkRunError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkTask {
    pub id: String,
    pub team_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub status: TeamWorkTaskStatus,
    #[serde(default)]
    pub priority: TeamWorkPriority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_slot_id: Option<String>,
    pub next_action_owner: TeamWorkNextActionOwner,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<TeamWorkLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_summary: Option<String>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub approval_state: TeamWorkApprovalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_reason: Option<TeamWorkQueueReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub exclusive_workspace: bool,
    pub version: u64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkRun {
    pub id: String,
    pub team_id: String,
    pub task_id: String,
    pub attempt: u32,
    pub slot_id: String,
    pub agent_backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status: TeamWorkRunStatus,
    pub queued_at: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_receipt: Option<TeamWorkVerificationReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TeamWorkRunUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TeamWorkRunError>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TeamWorkAction {
    ProvideInput,
    Approve,
    Reject,
    AcceptReview,
    ReturnForChanges,
    Retry,
    Reassign,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamWorkAttentionItem {
    pub task_id: String,
    pub status: TeamWorkTaskStatus,
    pub next_action_owner: TeamWorkNextActionOwner,
    pub reason: String,
    #[serde(default)]
    pub allowed_actions: Vec<TeamWorkAction>,
    pub requested_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkEvent {
    pub sequence: i64,
    pub event_id: String,
    pub team_id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub name: String,
    pub task_version: u64,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkSnapshot {
    pub team_id: String,
    pub sequence: i64,
    pub generated_at: TimestampMs,
    #[serde(default)]
    pub tasks: Vec<TeamWorkTask>,
    #[serde(default)]
    pub runs: Vec<TeamWorkRun>,
    #[serde(default)]
    pub attention: Vec<TeamWorkAttentionItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkEventBatch {
    pub team_id: String,
    pub after_sequence: i64,
    pub latest_sequence: i64,
    pub gap: bool,
    #[serde(default)]
    pub events: Vec<TeamWorkEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkCommandEnvelope {
    pub expected_version: u64,
    pub idempotency_key: String,
    pub actor: TeamWorkActor,
    pub command: TeamWorkCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateTeamWorkTaskRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub priority: TeamWorkPriority,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub exclusive_workspace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum TeamWorkCommand {
    Claim {
        slot_id: String,
        agent_backend: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        lease_duration_ms: u64,
    },
    Start,
    Heartbeat {
        lease_duration_ms: u64,
    },
    UpdateProgress {
        summary: String,
    },
    RequestInput {
        reason: String,
    },
    ProvideInput {
        summary: String,
    },
    RequestApproval {
        reason: String,
    },
    Approve {
        reason: String,
    },
    Reject {
        reason: String,
    },
    Block {
        reason: String,
        next_action_owner: TeamWorkNextActionOwner,
    },
    Unblock {
        reason: String,
    },
    SubmitForReview {
        output_summary: String,
        receipt: TeamWorkVerificationReceipt,
    },
    AcceptReview {
        reason: String,
    },
    ReturnForChanges {
        reason: String,
    },
    FailAttempt {
        error: TeamWorkRunError,
    },
    MarkStale {
        reason: String,
    },
    ActivateQueuedClaim {
        lease_duration_ms: u64,
    },
    Cancel {
        reason: String,
    },
    Reclaim {
        slot_id: String,
        agent_backend: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        lease_duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_ref: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TeamWorkErrorCode {
    InvalidTransition,
    VersionConflict,
    LeaseConflict,
    LeaseExpired,
    ActorForbidden,
    IdempotencyConflict,
    DependencyBlocked,
    RetryLimitReached,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamWorkCommandReceipt {
    pub idempotency_key: String,
    pub applied: bool,
    pub replayed: bool,
    pub event_sequence: i64,
    pub task: TeamWorkTask,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<TeamWorkRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_reason: Option<TeamWorkQueueReason>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_contract_is_tagged_and_stable() {
        let value = serde_json::to_value(TeamWorkCommand::Claim {
            slot_id: "agent-a".into(),
            agent_backend: "aionrs".into(),
            model: None,
            lease_duration_ms: 30_000,
        })
        .unwrap();

        assert_eq!(value["kind"], "claim");
        assert_eq!(value["payload"]["slot_id"], "agent-a");
        assert_eq!(value["payload"]["lease_duration_ms"], 30_000);
    }

    #[test]
    fn attention_and_terminal_statuses_do_not_overlap() {
        for status in [
            TeamWorkTaskStatus::NeedsInput,
            TeamWorkTaskStatus::NeedsApproval,
            TeamWorkTaskStatus::InReview,
            TeamWorkTaskStatus::Failed,
        ] {
            assert!(status.is_attention());
            assert!(!status.is_terminal());
        }
        assert!(TeamWorkTaskStatus::Done.is_terminal());
        assert!(TeamWorkTaskStatus::Cancelled.is_terminal());
    }
}
