use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_api_types::{
    CreateTeamWorkTaskRequest, TeamWorkAction, TeamWorkAttentionItem, TeamWorkCommand, TeamWorkCommandEnvelope,
    TeamWorkCommandReceipt, TeamWorkErrorCode, TeamWorkEvent, TeamWorkEventBatch, TeamWorkNextActionOwner,
    TeamWorkQueueReason, TeamWorkRun, TeamWorkSnapshot, TeamWorkTask, TeamWorkTaskStatus, WebSocketMessage,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    CreateTeamWorkTaskParams, ITeamWorkRepository, PersistTeamWorkCommandParams, PersistTeamWorkCommandResult,
    StoredTeamWorkRun, StoredTeamWorkTask,
};
use aionui_realtime::EventBroadcaster;
use dashmap::DashMap;
use tracing::warn;

use crate::TeamError;
use crate::work_kernel::{TeamWorkStateError, TeamWorkTaskAggregate};

const WORK_EVENT_NAME: &str = "team.workEvent";

pub struct TeamWorkService {
    repo: Arc<dyn ITeamWorkRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    capacity_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl TeamWorkService {
    pub fn new(repo: Arc<dyn ITeamWorkRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Arc<Self> {
        Arc::new(Self {
            repo,
            broadcaster,
            capacity_locks: DashMap::new(),
        })
    }

    pub async fn snapshot(&self, user_id: &str, team_id: &str) -> Result<TeamWorkSnapshot, TeamError> {
        let mut tasks = parse_tasks(self.repo.list_tasks(user_id, team_id).await?)?;
        project_reverse_dependencies(&mut tasks);
        let all_runs = self.repo.list_runs(user_id, team_id).await?;
        let runs = all_runs
            .into_iter()
            .map(|stored| serde_json::from_str(&stored.payload))
            .collect::<Result<Vec<TeamWorkRun>, _>>()?;
        let sequence = self.repo.latest_sequence(user_id, team_id).await?;
        let attention = tasks.iter().filter_map(attention_for_task).collect();
        Ok(TeamWorkSnapshot {
            team_id: team_id.to_owned(),
            sequence,
            generated_at: now_ms(),
            tasks,
            runs,
            attention,
        })
    }

    pub async fn events(
        &self,
        user_id: &str,
        team_id: &str,
        after_sequence: i64,
        limit: i64,
    ) -> Result<TeamWorkEventBatch, TeamError> {
        let latest_sequence = self.repo.latest_sequence(user_id, team_id).await?;
        let stored = self.repo.list_events(user_id, team_id, after_sequence, limit).await?;
        let gap = after_sequence > latest_sequence
            || stored
                .first()
                .is_some_and(|event| event.sequence != after_sequence.saturating_add(1));
        let events = stored
            .into_iter()
            .map(|event| {
                Ok(TeamWorkEvent {
                    sequence: event.sequence,
                    event_id: event.event_id,
                    team_id: event.team_id,
                    task_id: event.task_id,
                    run_id: event.run_id,
                    name: event.name,
                    task_version: event.task_version as u64,
                    payload: serde_json::from_str(&event.payload)?,
                    created_at: event.created_at,
                })
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        Ok(TeamWorkEventBatch {
            team_id: team_id.to_owned(),
            after_sequence,
            latest_sequence,
            gap,
            events,
        })
    }

    pub async fn create_task(
        &self,
        user_id: &str,
        team_id: &str,
        request: CreateTeamWorkTaskRequest,
    ) -> Result<TeamWorkTask, TeamError> {
        if request.subject.trim().is_empty() {
            return Err(TeamError::InvalidRequest("task subject is required".into()));
        }
        let task_id = request.id.unwrap_or_else(|| generate_prefixed_id("work_task"));
        let mut blocked_by = request.blocked_by;
        blocked_by.sort();
        blocked_by.dedup();
        if blocked_by.iter().any(|dependency| dependency == &task_id) {
            return Err(TeamError::InvalidRequest("task cannot depend on itself".into()));
        }

        let existing = parse_tasks(self.repo.list_tasks(user_id, team_id).await?)?;
        let by_id = existing
            .iter()
            .map(|task| (task.id.as_str(), task))
            .collect::<HashMap<_, _>>();
        for dependency in &blocked_by {
            if !by_id.contains_key(dependency.as_str()) {
                return Err(TeamError::InvalidRequest(format!(
                    "dependency task not found: {dependency}"
                )));
            }
        }
        if let Some(parent_id) = request.parent_id.as_deref()
            && !by_id.contains_key(parent_id)
        {
            return Err(TeamError::InvalidRequest(format!("parent task not found: {parent_id}")));
        }

        let now = now_ms();
        let status = if blocked_by
            .iter()
            .all(|dependency| by_id[dependency.as_str()].status == TeamWorkTaskStatus::Done)
        {
            TeamWorkTaskStatus::Ready
        } else {
            TeamWorkTaskStatus::Backlog
        };
        let task = TeamWorkTask {
            id: task_id,
            team_id: team_id.to_owned(),
            parent_id: request.parent_id,
            subject: request.subject.trim().to_owned(),
            description: request.description,
            acceptance_criteria: request.acceptance_criteria,
            status,
            priority: request.priority,
            owner_slot_id: None,
            next_action_owner: TeamWorkNextActionOwner::Agent,
            blocked_by,
            blocks: Vec::new(),
            current_run_id: None,
            lease: None,
            progress_summary: None,
            artifact_refs: Vec::new(),
            approval_state: Default::default(),
            queue_reason: None,
            workspace_key: request.workspace_key,
            exclusive_workspace: request.exclusive_workspace,
            version: 1,
            created_at: now,
            updated_at: now,
        };
        let mut graph_tasks = existing;
        graph_tasks.push(task.clone());
        validate_dependency_graph(&graph_tasks)?;
        let task_payload = serde_json::to_string(&task)?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "task": task,
            "run": null,
            "attention": attention_for_task(&task),
        }))?;
        let params = CreateTeamWorkTaskParams {
            task: stored_task(&task, task_payload)?,
            event_id: generate_prefixed_id("work_event"),
            event_name: "team.workTaskCreated".into(),
            event_payload,
        };
        let sequence = self.repo.create_task(user_id, &params).await?;
        self.broadcast_event(TeamWorkEvent {
            sequence,
            event_id: params.event_id,
            team_id: team_id.to_owned(),
            task_id: task.id.clone(),
            run_id: None,
            name: params.event_name,
            task_version: task.version,
            payload: serde_json::from_str(&params.event_payload)?,
            created_at: now,
        });
        Ok(task)
    }

    pub async fn apply_command(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
        envelope: TeamWorkCommandEnvelope,
    ) -> Result<TeamWorkCommandReceipt, TeamError> {
        let _capacity_guard = if matches!(
            envelope.command,
            TeamWorkCommand::Claim { .. }
                | TeamWorkCommand::Reclaim { .. }
                | TeamWorkCommand::ActivateQueuedClaim { .. }
                | TeamWorkCommand::ProvideInput { .. }
                | TeamWorkCommand::Approve { .. }
                | TeamWorkCommand::Unblock { .. }
        ) {
            Some(self.capacity_lock(team_id).lock_owned().await)
        } else {
            None
        };
        if let Some((stored_envelope, receipt)) = self
            .repo
            .get_command(user_id, team_id, task_id, &envelope.idempotency_key)
            .await?
        {
            let stored_envelope: TeamWorkCommandEnvelope = serde_json::from_str(&stored_envelope)?;
            if stored_envelope != envelope {
                return Err(TeamWorkStateError::new(
                    TeamWorkErrorCode::IdempotencyConflict,
                    "idempotency key was already used for a different command",
                )
                .into());
            }
            let mut receipt: TeamWorkCommandReceipt = serde_json::from_str(&receipt)?;
            receipt.replayed = true;
            return Ok(receipt);
        }
        let stored = self
            .repo
            .get_task(user_id, team_id, task_id)
            .await?
            .ok_or_else(|| TeamError::TaskNotFound(task_id.to_owned()))?;
        let task: TeamWorkTask = serde_json::from_str(&stored.payload)?;
        let runs = self
            .repo
            .list_runs(user_id, team_id)
            .await?
            .into_iter()
            .filter(|run| run.task_id == task_id)
            .map(|run| serde_json::from_str(&run.payload))
            .collect::<Result<Vec<TeamWorkRun>, _>>()?;
        let sequence = self.repo.latest_sequence(user_id, team_id).await?;
        let mut aggregate = TeamWorkTaskAggregate::new(task, runs, sequence);
        let at = now_ms();
        let capacity_queue = match &envelope.command {
            TeamWorkCommand::Claim {
                slot_id,
                agent_backend,
                model,
                ..
            }
            | TeamWorkCommand::Reclaim {
                slot_id,
                agent_backend,
                model,
                ..
            } => {
                let snapshot = self.snapshot(user_id, team_id).await?;
                capacity_reason(
                    &snapshot,
                    aggregate.task(),
                    slot_id,
                    agent_backend,
                    model.as_deref(),
                    at,
                )
            }
            TeamWorkCommand::ActivateQueuedClaim { .. } => {
                let run = aggregate
                    .task()
                    .current_run_id
                    .as_deref()
                    .and_then(|run_id| aggregate.runs().iter().find(|run| run.id == run_id))
                    .ok_or_else(|| TeamError::InvalidRequest("queued claim has no current run".into()))?;
                let snapshot = self.snapshot(user_id, team_id).await?;
                capacity_reason(
                    &snapshot,
                    aggregate.task(),
                    &run.slot_id,
                    &run.agent_backend,
                    run.model.as_deref(),
                    at,
                )
            }
            TeamWorkCommand::ProvideInput { .. }
            | TeamWorkCommand::Approve { .. }
            | TeamWorkCommand::Unblock { .. } => {
                let run = aggregate
                    .task()
                    .current_run_id
                    .as_deref()
                    .and_then(|run_id| aggregate.runs().iter().find(|run| run.id == run_id))
                    .ok_or_else(|| TeamError::InvalidRequest("paused task has no current run".into()))?;
                let snapshot = self.snapshot(user_id, team_id).await?;
                capacity_reason(
                    &snapshot,
                    aggregate.task(),
                    &run.slot_id,
                    &run.agent_backend,
                    run.model.as_deref(),
                    at,
                )
            }
            _ => None,
        };
        if matches!(
            envelope.command,
            TeamWorkCommand::ActivateQueuedClaim { .. }
                | TeamWorkCommand::ProvideInput { .. }
                | TeamWorkCommand::Approve { .. }
                | TeamWorkCommand::Unblock { .. }
        ) && capacity_queue.is_some()
        {
            return Err(
                TeamWorkStateError::new(TeamWorkErrorCode::LeaseConflict, "task capacity is not available").into(),
            );
        }
        let new_run_id = matches!(
            envelope.command,
            TeamWorkCommand::Claim { .. } | TeamWorkCommand::Reclaim { .. }
        )
        .then(|| generate_prefixed_id("work_run"));
        let expected_version = envelope.expected_version;
        let idempotency_key = envelope.idempotency_key.clone();
        let mut receipt = aggregate.apply(envelope.clone(), at, new_run_id).map_err(|error| {
            warn!(team_id, task_id, idempotency_key, code = ?error.code, "team work command rejected");
            TeamError::WorkState(error)
        })?;
        if let Some(reason) = capacity_queue {
            aggregate.queue_current_claim(reason)?;
            receipt.task = aggregate.task().clone();
            receipt.run = aggregate.runs().last().cloned();
            receipt.queue_reason = Some(reason);
        }
        let task = aggregate.task().clone();
        let latest_run = aggregate.runs().last().cloned();
        let event_id = generate_prefixed_id("work_event");
        let event_name = event_name(&envelope.command).to_owned();
        let task_payload = serde_json::to_string(&task)?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "task": task,
            "run": latest_run,
            "attention": attention_for_task(&task),
        }))?;
        let params = PersistTeamWorkCommandParams {
            user_id: user_id.to_owned(),
            task: stored_task(&task, task_payload.clone())?,
            expected_version,
            run: latest_run.as_ref().map(stored_run).transpose()?,
            idempotency_key,
            envelope: serde_json::to_string(&envelope)?,
            receipt: serde_json::to_string(&receipt)?,
            event_id: event_id.clone(),
            event_name: event_name.clone(),
            event_payload: event_payload.clone(),
            event_created_at: at,
        };
        match self.repo.persist_command(&params).await? {
            PersistTeamWorkCommandResult::Applied { sequence, receipt } => {
                let receipt: TeamWorkCommandReceipt = serde_json::from_str(&receipt)?;
                self.broadcast_event(TeamWorkEvent {
                    sequence,
                    event_id,
                    team_id: team_id.to_owned(),
                    task_id: task_id.to_owned(),
                    run_id: latest_run.map(|run| run.id),
                    name: event_name,
                    task_version: task.version,
                    payload: serde_json::from_str(&event_payload)?,
                    created_at: at,
                });
                if matches!(envelope.command, TeamWorkCommand::AcceptReview { .. }) {
                    for event in self.repo.unlock_ready_tasks(user_id, team_id, at).await? {
                        self.broadcast_event(TeamWorkEvent {
                            sequence: event.sequence,
                            event_id: event.event_id,
                            team_id: event.team_id,
                            task_id: event.task_id,
                            run_id: event.run_id,
                            name: event.name,
                            task_version: event.task_version as u64,
                            payload: serde_json::from_str(&event.payload)?,
                            created_at: event.created_at,
                        });
                    }
                }
                if matches!(
                    envelope.command,
                    TeamWorkCommand::SubmitForReview { .. }
                        | TeamWorkCommand::FailAttempt { .. }
                        | TeamWorkCommand::MarkStale { .. }
                        | TeamWorkCommand::Cancel { .. }
                        | TeamWorkCommand::Reject { .. }
                ) {
                    self.activate_queued_claims(user_id, team_id).await?;
                }
                Ok(receipt)
            }
            PersistTeamWorkCommandResult::Duplicate {
                envelope: stored_envelope,
                receipt,
            } => {
                let stored_envelope: TeamWorkCommandEnvelope = serde_json::from_str(&stored_envelope)?;
                if stored_envelope != envelope {
                    return Err(TeamWorkStateError::new(
                        TeamWorkErrorCode::IdempotencyConflict,
                        "idempotency key was already used for a different command",
                    )
                    .into());
                }
                let mut receipt: TeamWorkCommandReceipt = serde_json::from_str(&receipt)?;
                receipt.replayed = true;
                Ok(receipt)
            }
            PersistTeamWorkCommandResult::VersionConflict { actual_version } => Err(TeamWorkStateError::new(
                TeamWorkErrorCode::VersionConflict,
                format!("expected task version {expected_version}, found {actual_version}"),
            )
            .into()),
        }
    }

    pub async fn reconcile_stale(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<TeamWorkCommandReceipt>, TeamError> {
        let now = now_ms();
        let snapshot = self.snapshot(user_id, team_id).await?;
        let mut receipts = Vec::new();
        for task in snapshot.tasks.into_iter().filter(|task| {
            matches!(task.status, TeamWorkTaskStatus::Claimed | TeamWorkTaskStatus::Running)
                && task.lease.as_ref().is_some_and(|lease| lease.expires_at < now)
        }) {
            let run_id = task.current_run_id.clone().unwrap_or_else(|| "unknown".into());
            let expires_at = task.lease.as_ref().map(|lease| lease.expires_at).unwrap_or_default();
            let receipt = self
                .apply_command(
                    user_id,
                    team_id,
                    &task.id,
                    TeamWorkCommandEnvelope {
                        expected_version: task.version,
                        idempotency_key: format!("stale:{run_id}:{expires_at}"),
                        actor: aionui_api_types::TeamWorkActor {
                            kind: aionui_api_types::TeamWorkActorKind::System,
                            id: "lease-monitor".into(),
                        },
                        command: TeamWorkCommand::MarkStale {
                            reason: "lease_expired".into(),
                        },
                    },
                )
                .await?;
            warn!(
                team_id,
                task_id = task.id,
                run_id,
                "stale team work run recovered for retry"
            );
            receipts.push(receipt);
        }
        Ok(receipts)
    }

    async fn activate_queued_claims(&self, user_id: &str, team_id: &str) -> Result<(), TeamError> {
        loop {
            let snapshot = self.snapshot(user_id, team_id).await?;
            let candidate = snapshot.tasks.iter().find_map(|task| {
                let run = task
                    .queue_reason
                    .and(task.current_run_id.as_deref())
                    .and_then(|run_id| snapshot.runs.iter().find(|run| run.id == run_id))?;
                capacity_reason(
                    &snapshot,
                    task,
                    &run.slot_id,
                    &run.agent_backend,
                    run.model.as_deref(),
                    now_ms(),
                )
                .is_none()
                .then(|| (task.clone(), run.clone()))
            });
            let Some((task, run)) = candidate else {
                break;
            };
            let result = Box::pin(self.apply_command(
                user_id,
                team_id,
                &task.id,
                TeamWorkCommandEnvelope {
                    expected_version: task.version,
                    idempotency_key: format!("activate:{}:{}", run.id, task.version),
                    actor: aionui_api_types::TeamWorkActor {
                        kind: aionui_api_types::TeamWorkActorKind::System,
                        id: "capacity-scheduler".into(),
                    },
                    command: TeamWorkCommand::ActivateQueuedClaim {
                        lease_duration_ms: 30_000,
                    },
                },
            ))
            .await;
            if let Err(error) = result {
                match &error {
                    TeamError::WorkState(state_error)
                        if matches!(
                            state_error.code,
                            TeamWorkErrorCode::LeaseConflict | TeamWorkErrorCode::VersionConflict
                        ) =>
                    {
                        continue;
                    }
                    _ => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn capacity_lock(&self, team_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.capacity_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn broadcast_event(&self, event: TeamWorkEvent) {
        if let Ok(payload) = serde_json::to_value(event) {
            self.broadcaster
                .broadcast(WebSocketMessage::new(WORK_EVENT_NAME, payload));
        }
    }
}

fn parse_tasks(stored: Vec<StoredTeamWorkTask>) -> Result<Vec<TeamWorkTask>, serde_json::Error> {
    stored
        .into_iter()
        .map(|stored| serde_json::from_str(&stored.payload))
        .collect()
}

fn project_reverse_dependencies(tasks: &mut [TeamWorkTask]) {
    let mut reverse = HashMap::<String, Vec<String>>::new();
    for task in tasks.iter() {
        for dependency in &task.blocked_by {
            reverse.entry(dependency.clone()).or_default().push(task.id.clone());
        }
    }
    for task in tasks {
        task.blocks = reverse.remove(&task.id).unwrap_or_default();
    }
}

fn attention_for_task(task: &TeamWorkTask) -> Option<TeamWorkAttentionItem> {
    let allowed_actions = match task.status {
        TeamWorkTaskStatus::NeedsInput => vec![TeamWorkAction::ProvideInput],
        TeamWorkTaskStatus::NeedsApproval => vec![TeamWorkAction::Approve, TeamWorkAction::Reject],
        TeamWorkTaskStatus::InReview => vec![TeamWorkAction::AcceptReview, TeamWorkAction::ReturnForChanges],
        TeamWorkTaskStatus::Failed => vec![TeamWorkAction::Retry],
        _ => return None,
    };
    Some(TeamWorkAttentionItem {
        task_id: task.id.clone(),
        status: task.status,
        next_action_owner: task.next_action_owner,
        reason: task
            .progress_summary
            .clone()
            .unwrap_or_else(|| "action required".into()),
        allowed_actions,
        requested_at: task.updated_at,
    })
}

fn capacity_reason(
    snapshot: &TeamWorkSnapshot,
    target: &TeamWorkTask,
    slot_id: &str,
    agent_backend: &str,
    model: Option<&str>,
    now: i64,
) -> Option<TeamWorkQueueReason> {
    let active = snapshot
        .tasks
        .iter()
        .filter(|task| task.id != target.id && task.lease.as_ref().is_some_and(|lease| lease.expires_at >= now))
        .collect::<Vec<_>>();
    if active.len() >= 10 {
        return Some(TeamWorkQueueReason::TeamCapacity);
    }
    if active
        .iter()
        .any(|task| task.lease.as_ref().is_some_and(|lease| lease.holder == slot_id))
    {
        return Some(TeamWorkQueueReason::AgentCapacity);
    }
    let same_profile = active
        .iter()
        .filter_map(|task| task.current_run_id.as_deref())
        .filter_map(|run_id| snapshot.runs.iter().find(|run| run.id == run_id))
        .filter(|run| run.agent_backend == agent_backend && run.model.as_deref() == model)
        .count();
    if same_profile >= 4 {
        return Some(TeamWorkQueueReason::ProfileCapacity);
    }
    if let Some(workspace_key) = target.workspace_key.as_deref()
        && active.iter().any(|task| {
            task.workspace_key.as_deref() == Some(workspace_key)
                && (target.exclusive_workspace || task.exclusive_workspace)
        })
    {
        return Some(TeamWorkQueueReason::WorkspaceLocked);
    }
    None
}

fn stored_task(task: &TeamWorkTask, payload: String) -> Result<StoredTeamWorkTask, serde_json::Error> {
    Ok(StoredTeamWorkTask {
        id: task.id.clone(),
        team_id: task.team_id.clone(),
        status: serde_json::to_value(task.status)?
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        version: task.version as i64,
        blocked_by: serde_json::to_string(&task.blocked_by)?,
        payload,
        created_at: task.created_at,
        updated_at: task.updated_at,
    })
}

fn stored_run(run: &TeamWorkRun) -> Result<StoredTeamWorkRun, serde_json::Error> {
    let updated_at = run
        .ended_at
        .or(run.heartbeat_at)
        .or(run.started_at)
        .unwrap_or(run.queued_at);
    Ok(StoredTeamWorkRun {
        id: run.id.clone(),
        team_id: run.team_id.clone(),
        task_id: run.task_id.clone(),
        attempt: run.attempt as i64,
        status: serde_json::to_value(run.status)?
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        payload: serde_json::to_string(run)?,
        created_at: run.queued_at,
        updated_at,
    })
}

fn event_name(command: &TeamWorkCommand) -> &'static str {
    match command {
        TeamWorkCommand::Claim { .. } => "team.workTaskClaimed",
        TeamWorkCommand::Heartbeat { .. } => "team.workLeaseRenewed",
        TeamWorkCommand::RequestInput { .. } => "team.workInputRequested",
        TeamWorkCommand::RequestApproval { .. } => "team.workApprovalRequested",
        TeamWorkCommand::Approve { .. } | TeamWorkCommand::Reject { .. } => "team.workApprovalResolved",
        TeamWorkCommand::SubmitForReview { .. } => "team.workReviewSubmitted",
        TeamWorkCommand::AcceptReview { .. } | TeamWorkCommand::ReturnForChanges { .. } => "team.workReviewResolved",
        TeamWorkCommand::Reclaim { .. } => "team.workTaskReclaimed",
        TeamWorkCommand::MarkStale { .. } => "team.workRunStale",
        TeamWorkCommand::ActivateQueuedClaim { .. } => "team.workClaimActivated",
        _ => "team.workTaskChanged",
    }
}

pub fn validate_dependency_graph(tasks: &[TeamWorkTask]) -> Result<(), TeamError> {
    let dependencies = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.blocked_by.as_slice()))
        .collect::<HashMap<_, _>>();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for task in tasks {
        visit_dependency(&task.id, &dependencies, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_dependency<'a>(
    task_id: &'a str,
    dependencies: &HashMap<&'a str, &'a [String]>,
    visiting: &mut HashSet<&'a str>,
    visited: &mut HashSet<&'a str>,
) -> Result<(), TeamError> {
    if visited.contains(task_id) {
        return Ok(());
    }
    if !visiting.insert(task_id) {
        return Err(TeamError::InvalidRequest("task dependency cycle detected".into()));
    }
    for dependency in dependencies.get(task_id).copied().unwrap_or_default() {
        if dependencies.contains_key(dependency.as_str()) {
            visit_dependency(dependency, dependencies, visiting, visited)?;
        }
    }
    visiting.remove(task_id);
    visited.insert(task_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, blocked_by: &[&str]) -> TeamWorkTask {
        TeamWorkTask {
            id: id.into(),
            team_id: "team-1".into(),
            parent_id: None,
            subject: id.into(),
            description: None,
            acceptance_criteria: Vec::new(),
            status: TeamWorkTaskStatus::Backlog,
            priority: Default::default(),
            owner_slot_id: None,
            next_action_owner: TeamWorkNextActionOwner::Agent,
            blocked_by: blocked_by.iter().map(|id| (*id).into()).collect(),
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
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn dependency_graph_accepts_parallel_branches_and_rejects_cycles() {
        assert!(validate_dependency_graph(&[task("root", &[]), task("a", &["root"]), task("b", &["root"])]).is_ok());
        let error = validate_dependency_graph(&[task("a", &["b"]), task("b", &["a"])]).unwrap_err();
        assert!(error.to_string().contains("cycle"));
    }
}
