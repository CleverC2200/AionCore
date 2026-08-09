use std::collections::HashMap;

use aionui_api_types::{
    TeamWorkActorKind, TeamWorkApprovalState, TeamWorkCommand, TeamWorkCommandEnvelope, TeamWorkCommandReceipt,
    TeamWorkErrorCode, TeamWorkLease, TeamWorkNextActionOwner, TeamWorkQueueReason, TeamWorkRun, TeamWorkRunStatus,
    TeamWorkTask, TeamWorkTaskStatus,
};
use aionui_common::TimestampMs;
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct TeamWorkStateError {
    pub code: TeamWorkErrorCode,
    pub message: String,
}

impl TeamWorkStateError {
    pub(crate) fn new(code: TeamWorkErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct ProcessedCommand {
    envelope: TeamWorkCommandEnvelope,
    receipt: TeamWorkCommandReceipt,
}

struct BeginRun {
    id: String,
    slot_id: String,
    agent_backend: String,
    model: Option<String>,
    lease_duration_ms: u64,
    retry_of: Option<String>,
}

/// Pure aggregate for validating and applying the Team Work lifecycle.
///
/// Persistence supplies the clock and run identifier so replay tests remain deterministic.
#[derive(Debug, Clone)]
pub struct TeamWorkTaskAggregate {
    task: TeamWorkTask,
    runs: Vec<TeamWorkRun>,
    last_sequence: i64,
    processed_commands: HashMap<String, ProcessedCommand>,
}

impl TeamWorkTaskAggregate {
    pub fn new(task: TeamWorkTask, runs: Vec<TeamWorkRun>, last_sequence: i64) -> Self {
        Self {
            task,
            runs,
            last_sequence,
            processed_commands: HashMap::new(),
        }
    }

    pub fn task(&self) -> &TeamWorkTask {
        &self.task
    }

    pub fn runs(&self) -> &[TeamWorkRun] {
        &self.runs
    }

    pub fn last_sequence(&self) -> i64 {
        self.last_sequence
    }

    pub fn apply(
        &mut self,
        envelope: TeamWorkCommandEnvelope,
        at: TimestampMs,
        new_run_id: Option<String>,
    ) -> Result<TeamWorkCommandReceipt, TeamWorkStateError> {
        if let Some(processed) = self.processed_commands.get(&envelope.idempotency_key) {
            if processed.envelope != envelope {
                return Err(TeamWorkStateError::new(
                    TeamWorkErrorCode::IdempotencyConflict,
                    "idempotency key was already used for a different command",
                ));
            }
            let mut receipt = processed.receipt.clone();
            receipt.replayed = true;
            return Ok(receipt);
        }

        if envelope.expected_version != self.task.version {
            return Err(TeamWorkStateError::new(
                TeamWorkErrorCode::VersionConflict,
                format!(
                    "expected task version {}, found {}",
                    envelope.expected_version, self.task.version
                ),
            ));
        }

        self.apply_transition(&envelope, at, new_run_id)?;
        self.task.version = self.task.version.saturating_add(1);
        self.task.updated_at = at;
        self.last_sequence = self.last_sequence.saturating_add(1);

        let run = self
            .task
            .current_run_id
            .as_deref()
            .and_then(|run_id| self.runs.iter().find(|run| run.id == run_id))
            .cloned();
        let receipt = TeamWorkCommandReceipt {
            idempotency_key: envelope.idempotency_key.clone(),
            applied: true,
            replayed: false,
            event_sequence: self.last_sequence,
            task: self.task.clone(),
            run,
            queue_reason: self.task.queue_reason,
        };
        self.processed_commands.insert(
            envelope.idempotency_key.clone(),
            ProcessedCommand {
                envelope,
                receipt: receipt.clone(),
            },
        );
        Ok(receipt)
    }

    fn apply_transition(
        &mut self,
        envelope: &TeamWorkCommandEnvelope,
        at: TimestampMs,
        new_run_id: Option<String>,
    ) -> Result<(), TeamWorkStateError> {
        match &envelope.command {
            TeamWorkCommand::Claim {
                slot_id,
                agent_backend,
                model,
                lease_duration_ms,
            } => {
                self.require_actor(envelope, TeamWorkActorKind::Agent)?;
                self.require_actor_id(envelope, slot_id)?;
                self.require_status(&[TeamWorkTaskStatus::Ready])?;
                if !self.task.blocked_by.is_empty() {
                    return Err(TeamWorkStateError::new(
                        TeamWorkErrorCode::DependencyBlocked,
                        "task has incomplete dependencies",
                    ));
                }
                let run_id = require_new_run_id(new_run_id)?;
                self.begin_run(
                    BeginRun {
                        id: run_id,
                        slot_id: slot_id.clone(),
                        agent_backend: agent_backend.clone(),
                        model: model.clone(),
                        lease_duration_ms: *lease_duration_ms,
                        retry_of: None,
                    },
                    at,
                );
            }
            TeamWorkCommand::Start => {
                self.require_status(&[TeamWorkTaskStatus::Claimed])?;
                self.require_active_lease(envelope, at)?;
                let run = self.current_run_mut()?;
                run.status = TeamWorkRunStatus::Running;
                run.started_at = Some(at);
                run.heartbeat_at = Some(at);
                self.task.status = TeamWorkTaskStatus::Running;
            }
            TeamWorkCommand::Heartbeat { lease_duration_ms } => {
                self.require_status(&[TeamWorkTaskStatus::Claimed, TeamWorkTaskStatus::Running])?;
                self.require_active_lease(envelope, at)?;
                let lease = self.task.lease.as_mut().expect("active lease checked");
                lease.heartbeat_at = at;
                lease.expires_at = lease_expiry(at, *lease_duration_ms);
                self.current_run_mut()?.heartbeat_at = Some(at);
            }
            TeamWorkCommand::UpdateProgress { summary } => {
                self.require_status(&[TeamWorkTaskStatus::Running])?;
                self.require_active_lease(envelope, at)?;
                self.task.progress_summary = Some(summary.clone());
            }
            TeamWorkCommand::RequestInput { reason } => {
                self.pause_for_attention(envelope, at, TeamWorkTaskStatus::NeedsInput, reason)?;
                self.task.next_action_owner = TeamWorkNextActionOwner::Human;
            }
            TeamWorkCommand::ProvideInput { summary } => {
                self.require_actor(envelope, TeamWorkActorKind::Human)?;
                self.require_status(&[TeamWorkTaskStatus::NeedsInput])?;
                self.resume_current_run(at)?;
                self.task.progress_summary = Some(summary.clone());
            }
            TeamWorkCommand::RequestApproval { reason } => {
                self.pause_for_attention(envelope, at, TeamWorkTaskStatus::NeedsApproval, reason)?;
                self.task.approval_state = TeamWorkApprovalState::Pending;
                self.task.next_action_owner = TeamWorkNextActionOwner::Human;
            }
            TeamWorkCommand::Approve { reason } => {
                self.require_actor(envelope, TeamWorkActorKind::Human)?;
                self.require_status(&[TeamWorkTaskStatus::NeedsApproval])?;
                self.resume_current_run(at)?;
                self.task.approval_state = TeamWorkApprovalState::Approved;
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::Reject { reason } => {
                self.require_actor(envelope, TeamWorkActorKind::Human)?;
                self.require_status(&[TeamWorkTaskStatus::NeedsApproval])?;
                self.end_current_run(TeamWorkRunStatus::Cancelled, at)?;
                self.release_to_ready();
                self.task.approval_state = TeamWorkApprovalState::Rejected;
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::Block {
                reason,
                next_action_owner,
            } => {
                self.pause_for_attention(envelope, at, TeamWorkTaskStatus::Blocked, reason)?;
                self.task.next_action_owner = *next_action_owner;
            }
            TeamWorkCommand::Unblock { reason } => {
                self.require_non_agent_actor(envelope)?;
                self.require_status(&[TeamWorkTaskStatus::Blocked])?;
                self.resume_current_run(at)?;
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::SubmitForReview {
                output_summary,
                receipt,
            } => {
                self.require_status(&[TeamWorkTaskStatus::Running])?;
                self.require_active_lease(envelope, at)?;
                let run = self.current_run_mut()?;
                run.status = TeamWorkRunStatus::Completed;
                run.ended_at = Some(at);
                run.output_summary = Some(output_summary.clone());
                run.verification_receipt = Some(receipt.clone());
                self.task.status = TeamWorkTaskStatus::InReview;
                self.task.next_action_owner = TeamWorkNextActionOwner::Reviewer;
                self.task.owner_slot_id = None;
                self.task.lease = None;
                self.task.progress_summary = Some(output_summary.clone());
                self.task.artifact_refs = receipt.artifacts.clone();
            }
            TeamWorkCommand::AcceptReview { reason } => {
                self.require_reviewer(envelope)?;
                self.require_status(&[TeamWorkTaskStatus::InReview])?;
                self.task.status = TeamWorkTaskStatus::Done;
                self.task.next_action_owner = TeamWorkNextActionOwner::System;
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::ReturnForChanges { reason } => {
                self.require_reviewer(envelope)?;
                self.require_status(&[TeamWorkTaskStatus::InReview])?;
                self.release_to_ready();
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::FailAttempt { error } => {
                self.require_status(&[TeamWorkTaskStatus::Claimed, TeamWorkTaskStatus::Running])?;
                self.require_active_lease(envelope, at)?;
                let retry_limit_reached = self.runs.len() >= 3;
                let next_action_owner = if error.retryable && !retry_limit_reached {
                    TeamWorkNextActionOwner::Agent
                } else {
                    TeamWorkNextActionOwner::Human
                };
                let run = self.current_run_mut()?;
                run.status = TeamWorkRunStatus::Failed;
                run.ended_at = Some(at);
                run.error = Some(error.clone());
                self.task.status = TeamWorkTaskStatus::Failed;
                self.task.next_action_owner = next_action_owner;
                self.task.owner_slot_id = None;
                self.task.lease = None;
                self.task.progress_summary = Some(error.message.clone());
            }
            TeamWorkCommand::MarkStale { reason } => {
                self.require_actor(envelope, TeamWorkActorKind::System)?;
                self.require_status(&[TeamWorkTaskStatus::Claimed, TeamWorkTaskStatus::Running])?;
                self.end_current_run(TeamWorkRunStatus::Stale, at)?;
                self.task.status = TeamWorkTaskStatus::Failed;
                self.task.next_action_owner = if self.runs.len() >= 3 {
                    TeamWorkNextActionOwner::Human
                } else {
                    TeamWorkNextActionOwner::Agent
                };
                self.task.owner_slot_id = None;
                self.task.lease = None;
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::ActivateQueuedClaim { lease_duration_ms } => {
                self.require_actor(envelope, TeamWorkActorKind::System)?;
                self.require_status(&[TeamWorkTaskStatus::Claimed])?;
                if self.task.lease.is_some() || self.task.queue_reason.is_none() {
                    return Err(invalid_transition(self.task.status));
                }
                let holder = self.task.owner_slot_id.clone().ok_or_else(|| {
                    TeamWorkStateError::new(TeamWorkErrorCode::InvalidTransition, "queued claim has no owner")
                })?;
                self.task.lease = Some(TeamWorkLease {
                    holder,
                    expires_at: lease_expiry(at, *lease_duration_ms),
                    heartbeat_at: at,
                });
                self.task.queue_reason = None;
                self.task.next_action_owner = TeamWorkNextActionOwner::Agent;
                self.task.progress_summary = None;
            }
            TeamWorkCommand::Cancel { reason } => {
                self.require_non_agent_actor(envelope)?;
                if self.task.status.is_terminal() {
                    return Err(invalid_transition(self.task.status));
                }
                if self.task.current_run_id.is_some() {
                    self.end_current_run(TeamWorkRunStatus::Cancelled, at)?;
                }
                self.task.status = TeamWorkTaskStatus::Cancelled;
                self.task.next_action_owner = TeamWorkNextActionOwner::System;
                self.task.owner_slot_id = None;
                self.task.lease = None;
                self.task.progress_summary = Some(reason.clone());
            }
            TeamWorkCommand::Reclaim {
                slot_id,
                agent_backend,
                model,
                lease_duration_ms,
                resume_ref,
            } => {
                if envelope.actor.kind == TeamWorkActorKind::Agent {
                    self.require_actor_id(envelope, slot_id)?;
                } else {
                    self.require_actor(envelope, TeamWorkActorKind::Human)?;
                }
                if self.runs.len() >= 3 {
                    return Err(TeamWorkStateError::new(
                        TeamWorkErrorCode::RetryLimitReached,
                        "task reached the maximum of three attempts",
                    ));
                }
                self.require_status(&[TeamWorkTaskStatus::Failed])?;
                let run_id = require_new_run_id(new_run_id)?;
                let retry_of = self.task.current_run_id.clone();
                self.begin_run(
                    BeginRun {
                        id: run_id,
                        slot_id: slot_id.clone(),
                        agent_backend: agent_backend.clone(),
                        model: model.clone(),
                        lease_duration_ms: *lease_duration_ms,
                        retry_of,
                    },
                    at,
                );
                self.current_run_mut()?.resume_ref = resume_ref.clone();
            }
        }
        Ok(())
    }

    pub fn queue_current_claim(&mut self, reason: TeamWorkQueueReason) -> Result<(), TeamWorkStateError> {
        self.require_status(&[TeamWorkTaskStatus::Claimed])?;
        self.task.lease = None;
        self.task.queue_reason = Some(reason);
        self.task.next_action_owner = TeamWorkNextActionOwner::System;
        Ok(())
    }

    fn begin_run(&mut self, params: BeginRun, at: TimestampMs) {
        let attempt = self.runs.iter().map(|run| run.attempt).max().unwrap_or(0) + 1;
        self.runs.push(TeamWorkRun {
            id: params.id.clone(),
            team_id: self.task.team_id.clone(),
            task_id: self.task.id.clone(),
            attempt,
            slot_id: params.slot_id.clone(),
            agent_backend: params.agent_backend,
            model: params.model,
            status: TeamWorkRunStatus::Queued,
            queued_at: at,
            started_at: None,
            heartbeat_at: None,
            ended_at: None,
            retry_of: params.retry_of,
            resume_ref: None,
            output_summary: None,
            verification_receipt: None,
            usage: None,
            error: None,
        });
        self.task.status = TeamWorkTaskStatus::Claimed;
        self.task.owner_slot_id = Some(params.slot_id.clone());
        self.task.next_action_owner = TeamWorkNextActionOwner::Agent;
        self.task.current_run_id = Some(params.id);
        self.task.lease = Some(TeamWorkLease {
            holder: params.slot_id,
            expires_at: lease_expiry(at, params.lease_duration_ms),
            heartbeat_at: at,
        });
        self.task.queue_reason = None;
    }

    fn pause_for_attention(
        &mut self,
        envelope: &TeamWorkCommandEnvelope,
        at: TimestampMs,
        status: TeamWorkTaskStatus,
        reason: &str,
    ) -> Result<(), TeamWorkStateError> {
        self.require_status(&[TeamWorkTaskStatus::Running])?;
        self.require_active_lease(envelope, at)?;
        self.current_run_mut()?.status = TeamWorkRunStatus::Paused;
        self.task.status = status;
        self.task.progress_summary = Some(reason.to_owned());
        Ok(())
    }

    fn resume_current_run(&mut self, at: TimestampMs) -> Result<(), TeamWorkStateError> {
        let lease = self.task.lease.as_mut().ok_or_else(|| {
            TeamWorkStateError::new(TeamWorkErrorCode::LeaseConflict, "task has no active lease to resume")
        })?;
        lease.heartbeat_at = at;
        lease.expires_at = lease_expiry(at, 30_000);
        let run = self.current_run_mut()?;
        run.status = TeamWorkRunStatus::Running;
        run.heartbeat_at = Some(at);
        self.task.status = TeamWorkTaskStatus::Running;
        self.task.next_action_owner = TeamWorkNextActionOwner::Agent;
        Ok(())
    }

    fn release_to_ready(&mut self) {
        self.task.status = TeamWorkTaskStatus::Ready;
        self.task.next_action_owner = TeamWorkNextActionOwner::Agent;
        self.task.owner_slot_id = None;
        self.task.current_run_id = None;
        self.task.lease = None;
        self.task.queue_reason = None;
    }

    fn end_current_run(&mut self, status: TeamWorkRunStatus, at: TimestampMs) -> Result<(), TeamWorkStateError> {
        let run = self.current_run_mut()?;
        run.status = status;
        run.ended_at = Some(at);
        Ok(())
    }

    fn current_run_mut(&mut self) -> Result<&mut TeamWorkRun, TeamWorkStateError> {
        let run_id =
            self.task.current_run_id.as_deref().ok_or_else(|| {
                TeamWorkStateError::new(TeamWorkErrorCode::InvalidTransition, "task has no current run")
            })?;
        self.runs.iter_mut().find(|run| run.id == run_id).ok_or_else(|| {
            TeamWorkStateError::new(TeamWorkErrorCode::InvalidTransition, "task current run does not exist")
        })
    }

    fn require_status(&self, allowed: &[TeamWorkTaskStatus]) -> Result<(), TeamWorkStateError> {
        if allowed.contains(&self.task.status) {
            Ok(())
        } else {
            Err(invalid_transition(self.task.status))
        }
    }

    fn require_actor(
        &self,
        envelope: &TeamWorkCommandEnvelope,
        expected: TeamWorkActorKind,
    ) -> Result<(), TeamWorkStateError> {
        if envelope.actor.kind == expected {
            Ok(())
        } else {
            Err(TeamWorkStateError::new(
                TeamWorkErrorCode::ActorForbidden,
                "actor is not allowed to apply this command",
            ))
        }
    }

    fn require_actor_id(&self, envelope: &TeamWorkCommandEnvelope, expected: &str) -> Result<(), TeamWorkStateError> {
        if envelope.actor.id == expected {
            Ok(())
        } else {
            Err(TeamWorkStateError::new(
                TeamWorkErrorCode::ActorForbidden,
                "agent actor must match the target slot",
            ))
        }
    }

    fn require_non_agent_actor(&self, envelope: &TeamWorkCommandEnvelope) -> Result<(), TeamWorkStateError> {
        if matches!(
            envelope.actor.kind,
            TeamWorkActorKind::Human | TeamWorkActorKind::Reviewer | TeamWorkActorKind::System
        ) {
            Ok(())
        } else {
            Err(TeamWorkStateError::new(
                TeamWorkErrorCode::ActorForbidden,
                "human, reviewer, or system actor is required",
            ))
        }
    }

    fn require_reviewer(&self, envelope: &TeamWorkCommandEnvelope) -> Result<(), TeamWorkStateError> {
        if matches!(
            envelope.actor.kind,
            TeamWorkActorKind::Human | TeamWorkActorKind::Reviewer
        ) {
            Ok(())
        } else {
            Err(TeamWorkStateError::new(
                TeamWorkErrorCode::ActorForbidden,
                "human or reviewer actor is required",
            ))
        }
    }

    fn require_active_lease(
        &self,
        envelope: &TeamWorkCommandEnvelope,
        at: TimestampMs,
    ) -> Result<(), TeamWorkStateError> {
        self.require_actor(envelope, TeamWorkActorKind::Agent)?;
        let lease = self
            .task
            .lease
            .as_ref()
            .ok_or_else(|| TeamWorkStateError::new(TeamWorkErrorCode::LeaseConflict, "task has no active lease"))?;
        if lease.holder != envelope.actor.id {
            return Err(TeamWorkStateError::new(
                TeamWorkErrorCode::LeaseConflict,
                "agent does not hold the task lease",
            ));
        }
        if lease.expires_at < at {
            return Err(TeamWorkStateError::new(
                TeamWorkErrorCode::LeaseExpired,
                "task lease has expired",
            ));
        }
        Ok(())
    }
}

fn require_new_run_id(run_id: Option<String>) -> Result<String, TeamWorkStateError> {
    run_id.filter(|value| !value.trim().is_empty()).ok_or_else(|| {
        TeamWorkStateError::new(
            TeamWorkErrorCode::InvalidTransition,
            "claim and reclaim commands require a new run id",
        )
    })
}

fn lease_expiry(at: TimestampMs, duration_ms: u64) -> TimestampMs {
    at.saturating_add(duration_ms.min(i64::MAX as u64) as i64)
}

fn invalid_transition(status: TeamWorkTaskStatus) -> TeamWorkStateError {
    TeamWorkStateError::new(
        TeamWorkErrorCode::InvalidTransition,
        format!("command is not valid while task is {status:?}"),
    )
}

#[cfg(test)]
mod tests {
    use aionui_api_types::{TeamWorkActor, TeamWorkRunError, TeamWorkVerificationCheck, TeamWorkVerificationReceipt};

    use super::*;

    const START: TimestampMs = 1_000;

    fn ready_task(id: &str) -> TeamWorkTask {
        TeamWorkTask {
            id: id.into(),
            team_id: "team-1".into(),
            parent_id: None,
            subject: format!("Task {id}"),
            description: None,
            acceptance_criteria: vec!["scenario passes".into()],
            status: TeamWorkTaskStatus::Ready,
            priority: Default::default(),
            owner_slot_id: None,
            next_action_owner: TeamWorkNextActionOwner::Agent,
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            current_run_id: None,
            lease: None,
            progress_summary: None,
            artifact_refs: Vec::new(),
            approval_state: Default::default(),
            queue_reason: None,
            workspace_key: None,
            exclusive_workspace: false,
            version: 1,
            created_at: START,
            updated_at: START,
        }
    }

    fn agent(id: &str) -> TeamWorkActor {
        TeamWorkActor {
            kind: TeamWorkActorKind::Agent,
            id: id.into(),
        }
    }

    fn human() -> TeamWorkActor {
        TeamWorkActor {
            kind: TeamWorkActorKind::Human,
            id: "human-1".into(),
        }
    }

    fn reviewer() -> TeamWorkActor {
        TeamWorkActor {
            kind: TeamWorkActorKind::Reviewer,
            id: "reviewer-1".into(),
        }
    }

    fn apply(
        aggregate: &mut TeamWorkTaskAggregate,
        actor: TeamWorkActor,
        key: &str,
        command: TeamWorkCommand,
        at: TimestampMs,
        new_run_id: Option<&str>,
    ) -> TeamWorkCommandReceipt {
        aggregate
            .apply(
                TeamWorkCommandEnvelope {
                    expected_version: aggregate.task().version,
                    idempotency_key: key.into(),
                    actor,
                    command,
                },
                at,
                new_run_id.map(Into::into),
            )
            .unwrap()
    }

    fn claim(aggregate: &mut TeamWorkTaskAggregate, slot: &str, key: &str, run_id: &str, at: TimestampMs) {
        apply(
            aggregate,
            agent(slot),
            key,
            TeamWorkCommand::Claim {
                slot_id: slot.into(),
                agent_backend: "aionrs".into(),
                model: Some("test-model".into()),
                lease_duration_ms: 100_000,
            },
            at,
            Some(run_id),
        );
    }

    fn start(aggregate: &mut TeamWorkTaskAggregate, slot: &str, key: &str, at: TimestampMs) {
        apply(aggregate, agent(slot), key, TeamWorkCommand::Start, at, None);
    }

    fn submit_and_accept(aggregate: &mut TeamWorkTaskAggregate, slot: &str, prefix: &str, at: TimestampMs) {
        apply(
            aggregate,
            agent(slot),
            &format!("{prefix}-submit"),
            TeamWorkCommand::SubmitForReview {
                output_summary: "verified output".into(),
                receipt: TeamWorkVerificationReceipt {
                    checks: vec![TeamWorkVerificationCheck {
                        command: Some("cargo test".into()),
                        result: "passed".into(),
                        passed: true,
                    }],
                    artifacts: vec!["artifact://result".into()],
                    remaining_risks: Vec::new(),
                },
            },
            at,
            None,
        );
        apply(
            aggregate,
            reviewer(),
            &format!("{prefix}-accept"),
            TeamWorkCommand::AcceptReview {
                reason: "accepted".into(),
            },
            at + 1,
            None,
        );
    }

    #[test]
    fn three_agents_complete_input_approval_block_and_retry_scenario() {
        let mut task_a = TeamWorkTaskAggregate::new(ready_task("a"), Vec::new(), 10);
        claim(&mut task_a, "agent-a", "a-claim", "run-a1", START + 1);
        start(&mut task_a, "agent-a", "a-start", START + 2);
        apply(
            &mut task_a,
            agent("agent-a"),
            "a-input",
            TeamWorkCommand::RequestInput {
                reason: "need a value".into(),
            },
            START + 3,
            None,
        );
        apply(
            &mut task_a,
            human(),
            "a-input-answer",
            TeamWorkCommand::ProvideInput {
                summary: "value supplied".into(),
            },
            START + 4,
            None,
        );
        submit_and_accept(&mut task_a, "agent-a", "a", START + 5);

        let mut task_b = TeamWorkTaskAggregate::new(ready_task("b"), Vec::new(), 20);
        claim(&mut task_b, "agent-b", "b-claim", "run-b1", START + 1);
        start(&mut task_b, "agent-b", "b-start", START + 2);
        apply(
            &mut task_b,
            agent("agent-b"),
            "b-block",
            TeamWorkCommand::Block {
                reason: "waiting for access".into(),
                next_action_owner: TeamWorkNextActionOwner::Human,
            },
            START + 3,
            None,
        );
        apply(
            &mut task_b,
            human(),
            "b-unblock",
            TeamWorkCommand::Unblock {
                reason: "access granted".into(),
            },
            START + 4,
            None,
        );
        submit_and_accept(&mut task_b, "agent-b", "b", START + 5);

        let mut task_c = TeamWorkTaskAggregate::new(ready_task("c"), Vec::new(), 30);
        claim(&mut task_c, "agent-c", "c-claim", "run-c1", START + 1);
        start(&mut task_c, "agent-c", "c-start", START + 2);
        apply(
            &mut task_c,
            agent("agent-c"),
            "c-approval",
            TeamWorkCommand::RequestApproval {
                reason: "approve deployment".into(),
            },
            START + 3,
            None,
        );
        let approval_envelope = TeamWorkCommandEnvelope {
            expected_version: task_c.task().version,
            idempotency_key: "c-approved".into(),
            actor: human(),
            command: TeamWorkCommand::Approve {
                reason: "approved".into(),
            },
        };
        let approval = task_c.apply(approval_envelope.clone(), START + 4, None).unwrap();
        let replay = task_c.apply(approval_envelope, START + 4, None).unwrap();
        assert!(!approval.replayed);
        assert!(replay.replayed);
        assert_eq!(approval.task.version, replay.task.version);

        apply(
            &mut task_c,
            agent("agent-c"),
            "c-fail",
            TeamWorkCommand::FailAttempt {
                error: TeamWorkRunError {
                    code: "transient".into(),
                    message: "temporary failure".into(),
                    retryable: true,
                },
            },
            START + 5,
            None,
        );
        apply(
            &mut task_c,
            agent("agent-c"),
            "c-reclaim",
            TeamWorkCommand::Reclaim {
                slot_id: "agent-c".into(),
                agent_backend: "aionrs".into(),
                model: Some("test-model".into()),
                lease_duration_ms: 100_000,
                resume_ref: Some("checkpoint://c".into()),
            },
            START + 6,
            Some("run-c2"),
        );
        start(&mut task_c, "agent-c", "c-restart", START + 7);
        submit_and_accept(&mut task_c, "agent-c", "c", START + 8);

        assert_eq!(task_a.task().status, TeamWorkTaskStatus::Done);
        assert_eq!(task_b.task().status, TeamWorkTaskStatus::Done);
        assert_eq!(task_c.task().status, TeamWorkTaskStatus::Done);
        assert_eq!(task_c.runs().len(), 2);
        assert_eq!(task_c.runs()[1].retry_of.as_deref(), Some("run-c1"));
        assert_eq!(task_c.runs()[1].resume_ref.as_deref(), Some("checkpoint://c"));
    }

    #[test]
    fn optimistic_version_allows_only_one_claim() {
        let initial = ready_task("claim-race");
        let envelope = TeamWorkCommandEnvelope {
            expected_version: initial.version,
            idempotency_key: "claim-a".into(),
            actor: agent("agent-a"),
            command: TeamWorkCommand::Claim {
                slot_id: "agent-a".into(),
                agent_backend: "aionrs".into(),
                model: None,
                lease_duration_ms: 10_000,
            },
        };
        let mut aggregate = TeamWorkTaskAggregate::new(initial, Vec::new(), 0);
        aggregate.apply(envelope, START + 1, Some("run-a".into())).unwrap();

        let error = aggregate
            .apply(
                TeamWorkCommandEnvelope {
                    expected_version: 1,
                    idempotency_key: "claim-b".into(),
                    actor: agent("agent-b"),
                    command: TeamWorkCommand::Claim {
                        slot_id: "agent-b".into(),
                        agent_backend: "aionrs".into(),
                        model: None,
                        lease_duration_ms: 10_000,
                    },
                },
                START + 1,
                Some("run-b".into()),
            )
            .unwrap_err();

        assert_eq!(error.code, TeamWorkErrorCode::VersionConflict);
        assert_eq!(aggregate.runs().len(), 1);
    }

    #[test]
    fn commands_reject_wrong_actor_expired_lease_and_idempotency_reuse() {
        let mut aggregate = TeamWorkTaskAggregate::new(ready_task("guards"), Vec::new(), 0);
        claim(&mut aggregate, "agent-a", "claim", "run-a", START + 1);

        let wrong_actor = aggregate
            .apply(
                TeamWorkCommandEnvelope {
                    expected_version: aggregate.task().version,
                    idempotency_key: "wrong-actor".into(),
                    actor: agent("agent-b"),
                    command: TeamWorkCommand::Start,
                },
                START + 2,
                None,
            )
            .unwrap_err();
        assert_eq!(wrong_actor.code, TeamWorkErrorCode::LeaseConflict);

        let conflict = aggregate
            .apply(
                TeamWorkCommandEnvelope {
                    expected_version: aggregate.task().version,
                    idempotency_key: "claim".into(),
                    actor: agent("agent-a"),
                    command: TeamWorkCommand::Start,
                },
                START + 2,
                None,
            )
            .unwrap_err();
        assert_eq!(conflict.code, TeamWorkErrorCode::IdempotencyConflict);

        let expired = aggregate
            .apply(
                TeamWorkCommandEnvelope {
                    expected_version: aggregate.task().version,
                    idempotency_key: "expired".into(),
                    actor: agent("agent-a"),
                    command: TeamWorkCommand::Start,
                },
                START + 200_000,
                None,
            )
            .unwrap_err();
        assert_eq!(expired.code, TeamWorkErrorCode::LeaseExpired);
    }

    #[test]
    fn heartbeat_only_renews_the_current_lease() {
        let mut aggregate = TeamWorkTaskAggregate::new(ready_task("heartbeat"), Vec::new(), 0);
        claim(&mut aggregate, "agent-a", "claim", "run-a", START + 1);
        start(&mut aggregate, "agent-a", "start", START + 2);
        apply(
            &mut aggregate,
            agent("agent-a"),
            "progress",
            TeamWorkCommand::UpdateProgress {
                summary: "half complete".into(),
            },
            START + 3,
            None,
        );

        apply(
            &mut aggregate,
            agent("agent-a"),
            "heartbeat",
            TeamWorkCommand::Heartbeat {
                lease_duration_ms: 20_000,
            },
            START + 4,
            None,
        );

        let lease = aggregate.task().lease.as_ref().expect("lease renewed");
        assert_eq!(lease.heartbeat_at, START + 4);
        assert_eq!(lease.expires_at, START + 20_004);
        assert_eq!(aggregate.task().progress_summary.as_deref(), Some("half complete"));
        assert_eq!(aggregate.task().status, TeamWorkTaskStatus::Running);
    }

    #[test]
    fn human_attention_can_resume_after_the_previous_lease_expires() {
        let mut aggregate = TeamWorkTaskAggregate::new(ready_task("slow-input"), Vec::new(), 0);
        claim(&mut aggregate, "agent-a", "claim", "run-a", START + 1);
        start(&mut aggregate, "agent-a", "start", START + 2);
        apply(
            &mut aggregate,
            agent("agent-a"),
            "request-input",
            TeamWorkCommand::RequestInput {
                reason: "waiting for a human".into(),
            },
            START + 3,
            None,
        );

        apply(
            &mut aggregate,
            human(),
            "provide-input",
            TeamWorkCommand::ProvideInput {
                summary: "answer supplied".into(),
            },
            START + 200_000,
            None,
        );

        assert_eq!(aggregate.task().status, TeamWorkTaskStatus::Running);
        let lease = aggregate.task().lease.as_ref().expect("lease renewed");
        assert_eq!(lease.heartbeat_at, START + 200_000);
        assert_eq!(lease.expires_at, START + 230_000);
    }

    #[test]
    fn retry_limit_stops_after_three_distinct_attempts() {
        let mut aggregate = TeamWorkTaskAggregate::new(ready_task("retry-limit"), Vec::new(), 0);
        claim(&mut aggregate, "agent-a", "claim-1", "run-1", START + 1);

        for attempt in 1..=3 {
            start(
                &mut aggregate,
                "agent-a",
                &format!("start-{attempt}"),
                START + attempt * 10,
            );
            apply(
                &mut aggregate,
                agent("agent-a"),
                &format!("fail-{attempt}"),
                TeamWorkCommand::FailAttempt {
                    error: TeamWorkRunError {
                        code: "transient".into(),
                        message: format!("attempt {attempt} failed"),
                        retryable: true,
                    },
                },
                START + attempt * 10 + 1,
                None,
            );
            if attempt < 3 {
                apply(
                    &mut aggregate,
                    agent("agent-a"),
                    &format!("reclaim-{attempt}"),
                    TeamWorkCommand::Reclaim {
                        slot_id: "agent-a".into(),
                        agent_backend: "aionrs".into(),
                        model: Some("test-model".into()),
                        lease_duration_ms: 100_000,
                        resume_ref: None,
                    },
                    START + attempt * 10 + 2,
                    Some(&format!("run-{}", attempt + 1)),
                );
            }
        }

        assert_eq!(aggregate.runs().len(), 3);
        assert_eq!(aggregate.task().next_action_owner, TeamWorkNextActionOwner::Human);
        let error = aggregate
            .apply(
                TeamWorkCommandEnvelope {
                    expected_version: aggregate.task().version,
                    idempotency_key: "reclaim-4".into(),
                    actor: agent("agent-a"),
                    command: TeamWorkCommand::Reclaim {
                        slot_id: "agent-a".into(),
                        agent_backend: "aionrs".into(),
                        model: Some("test-model".into()),
                        lease_duration_ms: 100_000,
                        resume_ref: None,
                    },
                },
                START + 40,
                Some("run-4".into()),
            )
            .unwrap_err();
        assert_eq!(error.code, TeamWorkErrorCode::RetryLimitReached);
    }
}
