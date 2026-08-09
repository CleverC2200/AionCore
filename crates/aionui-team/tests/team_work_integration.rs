use std::sync::Arc;

use aionui_api_types::{
    CreateTeamWorkTaskRequest, TeamWorkActor, TeamWorkActorKind, TeamWorkCommand, TeamWorkCommandEnvelope,
    TeamWorkPriority, TeamWorkTaskStatus, TeamWorkVerificationCheck, TeamWorkVerificationReceipt,
};
use aionui_common::now_ms;
use aionui_db::models::TeamRow;
use aionui_db::{ITeamRepository, SqliteTeamRepository, SqliteTeamWorkRepository, init_database_memory};
use aionui_realtime::BroadcastEventBus;
use aionui_team::{TeamError, TeamWorkService};
use futures_util::future::join_all;

const USER_ID: &str = "system_default_user";

fn team() -> TeamRow {
    let now = now_ms();
    TeamRow {
        id: "team-work-test".into(),
        user_id: USER_ID.into(),
        name: "Work test".into(),
        workspace: "/tmp/team-work-test".into(),
        workspace_mode: "shared".into(),
        agents: "[]".into(),
        lead_agent_id: None,
        session_mode: None,
        agents_version: "1.0.1".into(),
        created_at: now,
        updated_at: now,
        project_id: None,
        folder_id: None,
    }
}

fn claim(actor: &str, key: &str) -> TeamWorkCommandEnvelope {
    TeamWorkCommandEnvelope {
        expected_version: 1,
        idempotency_key: key.into(),
        actor: TeamWorkActor {
            kind: TeamWorkActorKind::Agent,
            id: actor.into(),
        },
        command: TeamWorkCommand::Claim {
            slot_id: actor.into(),
            agent_backend: "aionrs".into(),
            model: None,
            lease_duration_ms: 30_000,
        },
    }
}

fn command(
    version: u64,
    key: &str,
    kind: TeamWorkActorKind,
    actor: &str,
    command: TeamWorkCommand,
) -> TeamWorkCommandEnvelope {
    TeamWorkCommandEnvelope {
        expected_version: version,
        idempotency_key: key.into(),
        actor: TeamWorkActor { kind, id: actor.into() },
        command,
    }
}

async fn create_work_task(service: &TeamWorkService, id: &str, workspace_key: Option<&str>, exclusive_workspace: bool) {
    service
        .create_task(
            USER_ID,
            "team-work-test",
            CreateTeamWorkTaskRequest {
                id: Some(id.into()),
                parent_id: None,
                subject: id.into(),
                description: None,
                acceptance_criteria: Vec::new(),
                priority: TeamWorkPriority::Normal,
                blocked_by: Vec::new(),
                workspace_key: workspace_key.map(Into::into),
                exclusive_workspace,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_claim_is_atomic_and_events_reconcile_to_snapshot() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(16)),
    );
    let task = service
        .create_task(
            USER_ID,
            "team-work-test",
            CreateTeamWorkTaskRequest {
                id: Some("task-1".into()),
                parent_id: None,
                subject: "Atomic claim".into(),
                description: None,
                acceptance_criteria: vec!["one owner".into()],
                priority: TeamWorkPriority::Normal,
                blocked_by: Vec::new(),
                workspace_key: None,
                exclusive_workspace: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.status, TeamWorkTaskStatus::Ready);

    let claim_a = claim("agent-a", "claim-a");
    let claim_b = claim("agent-b", "claim-b");
    let (result_a, result_b) = tokio::join!(
        service.apply_command(USER_ID, "team-work-test", "task-1", claim_a.clone()),
        service.apply_command(USER_ID, "team-work-test", "task-1", claim_b.clone()),
    );
    assert_eq!(usize::from(result_a.is_ok()) + usize::from(result_b.is_ok()), 1);
    let rejected = if result_a.is_err() {
        result_a.as_ref().unwrap_err()
    } else {
        result_b.as_ref().unwrap_err()
    };
    assert!(matches!(rejected, TeamError::WorkState(_)));

    let (winning_envelope, first_receipt) = if let Ok(receipt) = result_a {
        (claim_a, receipt)
    } else {
        (claim_b, result_b.unwrap())
    };
    let replay = service
        .apply_command(USER_ID, "team-work-test", "task-1", winning_envelope)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.event_sequence, first_receipt.event_sequence);

    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    assert_eq!(snapshot.sequence, 2);
    assert_eq!(snapshot.tasks[0].status, TeamWorkTaskStatus::Claimed);
    assert!(snapshot.tasks[0].owner_slot_id.is_some());
    assert_eq!(snapshot.runs.len(), 1);

    let events = service.events(USER_ID, "team-work-test", 0, 20).await.unwrap();
    assert!(!events.gap);
    assert_eq!(
        events.events.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        service
            .snapshot("another-user", "team-work-test")
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
}

#[tokio::test]
async fn approval_review_receipt_and_dependency_unlock_are_auditable() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(32)),
    );
    for (id, blocked_by) in [("root", Vec::new()), ("child", vec!["root".into()])] {
        service
            .create_task(
                USER_ID,
                "team-work-test",
                CreateTeamWorkTaskRequest {
                    id: Some(id.into()),
                    parent_id: None,
                    subject: id.into(),
                    description: None,
                    acceptance_criteria: vec!["verified".into()],
                    priority: TeamWorkPriority::Normal,
                    blocked_by,
                    workspace_key: None,
                    exclusive_workspace: false,
                },
            )
            .await
            .unwrap();
    }
    let child = service
        .snapshot(USER_ID, "team-work-test")
        .await
        .unwrap()
        .tasks
        .into_iter()
        .find(|task| task.id == "child")
        .unwrap();
    assert_eq!(child.status, TeamWorkTaskStatus::Backlog);

    service
        .apply_command(USER_ID, "team-work-test", "root", claim("agent-a", "root-claim"))
        .await
        .unwrap();
    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "root",
            command(
                2,
                "root-start",
                TeamWorkActorKind::Agent,
                "agent-a",
                TeamWorkCommand::Start,
            ),
        )
        .await
        .unwrap();
    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "root",
            command(
                3,
                "root-approval",
                TeamWorkActorKind::Agent,
                "agent-a",
                TeamWorkCommand::RequestApproval {
                    reason: "approve protected operation".into(),
                },
            ),
        )
        .await
        .unwrap();
    let approval = command(
        4,
        "root-approved",
        TeamWorkActorKind::Human,
        USER_ID,
        TeamWorkCommand::Approve {
            reason: "approved".into(),
        },
    );
    service
        .apply_command(USER_ID, "team-work-test", "root", approval.clone())
        .await
        .unwrap();
    assert!(
        service
            .apply_command(USER_ID, "team-work-test", "root", approval)
            .await
            .unwrap()
            .replayed
    );
    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "root",
            command(
                5,
                "root-submit",
                TeamWorkActorKind::Agent,
                "agent-a",
                TeamWorkCommand::SubmitForReview {
                    output_summary: "result ready".into(),
                    receipt: TeamWorkVerificationReceipt {
                        checks: vec![TeamWorkVerificationCheck {
                            command: Some("cargo test".into()),
                            result: "passed".into(),
                            passed: true,
                        }],
                        artifacts: vec!["artifact://root".into()],
                        remaining_risks: vec!["manual rollout".into()],
                    },
                },
            ),
        )
        .await
        .unwrap();
    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "root",
            command(
                6,
                "root-accepted",
                TeamWorkActorKind::Reviewer,
                "reviewer-a",
                TeamWorkCommand::AcceptReview {
                    reason: "accepted".into(),
                },
            ),
        )
        .await
        .unwrap();

    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    assert_eq!(
        snapshot.tasks.iter().find(|task| task.id == "root").unwrap().status,
        TeamWorkTaskStatus::Done
    );
    assert_eq!(
        snapshot.tasks.iter().find(|task| task.id == "child").unwrap().status,
        TeamWorkTaskStatus::Ready
    );
    let run = snapshot.runs.iter().find(|run| run.task_id == "root").unwrap();
    assert_eq!(
        run.verification_receipt.as_ref().unwrap().artifacts,
        vec!["artifact://root"]
    );
    let events = service.events(USER_ID, "team-work-test", 0, 50).await.unwrap();
    assert_eq!(events.latest_sequence, 9);
    assert_eq!(events.events.last().unwrap().name, "team.workTaskUnlocked");
}

#[tokio::test]
async fn expired_lease_becomes_stale_before_a_new_attempt_is_reclaimed() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(16)),
    );
    service
        .create_task(
            USER_ID,
            "team-work-test",
            CreateTeamWorkTaskRequest {
                id: Some("stale-task".into()),
                parent_id: None,
                subject: "Recover".into(),
                description: None,
                acceptance_criteria: Vec::new(),
                priority: TeamWorkPriority::Normal,
                blocked_by: Vec::new(),
                workspace_key: None,
                exclusive_workspace: false,
            },
        )
        .await
        .unwrap();
    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "stale-task",
            command(
                1,
                "stale-claim",
                TeamWorkActorKind::Agent,
                "agent-a",
                TeamWorkCommand::Claim {
                    slot_id: "agent-a".into(),
                    agent_backend: "aionrs".into(),
                    model: None,
                    lease_duration_ms: 1,
                },
            ),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(
        service.reconcile_stale(USER_ID, "team-work-test").await.unwrap().len(),
        1
    );
    assert!(
        service
            .reconcile_stale(USER_ID, "team-work-test")
            .await
            .unwrap()
            .is_empty()
    );

    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "stale-task",
            command(
                3,
                "stale-reclaim",
                TeamWorkActorKind::Human,
                USER_ID,
                TeamWorkCommand::Reclaim {
                    slot_id: "agent-a".into(),
                    agent_backend: "aionrs".into(),
                    model: None,
                    lease_duration_ms: 30_000,
                    resume_ref: Some("same-session-same-workspace".into()),
                },
            ),
        )
        .await
        .unwrap();
    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    assert_eq!(snapshot.tasks[0].status, TeamWorkTaskStatus::Claimed);
    assert_eq!(snapshot.runs.len(), 2);
    assert_eq!(snapshot.runs[0].status, aionui_api_types::TeamWorkRunStatus::Stale);
    assert_eq!(snapshot.runs[1].retry_of.as_deref(), Some(snapshot.runs[0].id.as_str()));
}

#[tokio::test]
async fn agent_and_workspace_capacity_queue_then_activate_after_release() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(32)),
    );
    create_work_task(&service, "active", Some("workspace-a"), true).await;
    create_work_task(&service, "agent-queued", Some("workspace-b"), false).await;
    create_work_task(&service, "workspace-queued", Some("workspace-a"), true).await;

    service
        .apply_command(USER_ID, "team-work-test", "active", claim("agent-a", "active-claim"))
        .await
        .unwrap();
    let agent_queued = service
        .apply_command(
            USER_ID,
            "team-work-test",
            "agent-queued",
            command(
                1,
                "agent-queued-claim",
                TeamWorkActorKind::Agent,
                "agent-a",
                TeamWorkCommand::Claim {
                    slot_id: "agent-a".into(),
                    agent_backend: "different-profile".into(),
                    model: None,
                    lease_duration_ms: 30_000,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        agent_queued.queue_reason,
        Some(aionui_api_types::TeamWorkQueueReason::AgentCapacity)
    );
    assert!(agent_queued.task.lease.is_none());

    let workspace_queued = service
        .apply_command(
            USER_ID,
            "team-work-test",
            "workspace-queued",
            command(
                1,
                "workspace-queued-claim",
                TeamWorkActorKind::Agent,
                "agent-b",
                TeamWorkCommand::Claim {
                    slot_id: "agent-b".into(),
                    agent_backend: "workspace-profile".into(),
                    model: None,
                    lease_duration_ms: 30_000,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        workspace_queued.queue_reason,
        Some(aionui_api_types::TeamWorkQueueReason::WorkspaceLocked)
    );

    service
        .apply_command(
            USER_ID,
            "team-work-test",
            "active",
            command(
                2,
                "active-cancel",
                TeamWorkActorKind::Human,
                USER_ID,
                TeamWorkCommand::Cancel {
                    reason: "release capacity".into(),
                },
            ),
        )
        .await
        .unwrap();
    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    let activated = snapshot
        .tasks
        .iter()
        .filter(|task| task.id != "active" && task.lease.is_some())
        .count();
    assert_eq!(activated, 2);
    assert!(
        snapshot
            .tasks
            .iter()
            .filter(|task| task.id != "active")
            .all(|task| task.queue_reason.is_none())
    );
}

#[tokio::test]
async fn profile_and_team_capacity_keep_excess_claims_in_the_capacity_queue() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(64)),
    );
    for index in 0..12 {
        create_work_task(&service, &format!("task-{index}"), None, false).await;
    }
    for index in 0..4 {
        service
            .apply_command(
                USER_ID,
                "team-work-test",
                &format!("task-{index}"),
                command(
                    1,
                    &format!("claim-{index}"),
                    TeamWorkActorKind::Agent,
                    &format!("agent-{index}"),
                    TeamWorkCommand::Claim {
                        slot_id: format!("agent-{index}"),
                        agent_backend: "shared-profile".into(),
                        model: Some("shared-model".into()),
                        lease_duration_ms: 30_000,
                    },
                ),
            )
            .await
            .unwrap();
    }
    let profile_queued = service
        .apply_command(
            USER_ID,
            "team-work-test",
            "task-4",
            command(
                1,
                "claim-4",
                TeamWorkActorKind::Agent,
                "agent-4",
                TeamWorkCommand::Claim {
                    slot_id: "agent-4".into(),
                    agent_backend: "shared-profile".into(),
                    model: Some("shared-model".into()),
                    lease_duration_ms: 30_000,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        profile_queued.queue_reason,
        Some(aionui_api_types::TeamWorkQueueReason::ProfileCapacity)
    );

    for index in 5..12 {
        let receipt = service
            .apply_command(
                USER_ID,
                "team-work-test",
                &format!("task-{index}"),
                command(
                    1,
                    &format!("claim-{index}"),
                    TeamWorkActorKind::Agent,
                    &format!("agent-{index}"),
                    TeamWorkCommand::Claim {
                        slot_id: format!("agent-{index}"),
                        agent_backend: format!("profile-{index}"),
                        model: None,
                        lease_duration_ms: 30_000,
                    },
                ),
            )
            .await
            .unwrap();
        if index < 11 {
            assert!(receipt.queue_reason.is_none());
        } else {
            assert_eq!(
                receipt.queue_reason,
                Some(aionui_api_types::TeamWorkQueueReason::TeamCapacity)
            );
        }
    }
    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    assert_eq!(snapshot.tasks.iter().filter(|task| task.lease.is_some()).count(), 10);
    assert_eq!(
        snapshot.tasks.iter().filter(|task| task.queue_reason.is_some()).count(),
        2
    );
}

#[tokio::test]
async fn ten_parallel_tasks_receive_ten_distinct_runs_and_single_leases() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(64)),
    );
    for index in 0..10 {
        create_work_task(
            &service,
            &format!("parallel-{index}"),
            Some(&format!("worktree-{index}")),
            true,
        )
        .await;
    }
    let claims = join_all((0..10).map(|index| {
        let service = service.clone();
        async move {
            service
                .apply_command(
                    USER_ID,
                    "team-work-test",
                    &format!("parallel-{index}"),
                    command(
                        1,
                        &format!("parallel-claim-{index}"),
                        TeamWorkActorKind::Agent,
                        &format!("parallel-agent-{index}"),
                        TeamWorkCommand::Claim {
                            slot_id: format!("parallel-agent-{index}"),
                            agent_backend: format!("parallel-profile-{index}"),
                            model: None,
                            lease_duration_ms: 30_000,
                        },
                    ),
                )
                .await
        }
    }))
    .await;
    assert!(
        claims
            .iter()
            .all(|result| result.as_ref().is_ok_and(|receipt| receipt.queue_reason.is_none()))
    );

    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    assert_eq!(snapshot.tasks.iter().filter(|task| task.lease.is_some()).count(), 10);
    assert_eq!(snapshot.runs.len(), 10);
    assert_eq!(
        snapshot
            .runs
            .iter()
            .map(|run| run.id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        10
    );
}

#[tokio::test]
async fn concurrent_claims_cannot_oversubscribe_the_team_capacity() {
    let db = init_database_memory().await.unwrap();
    SqliteTeamRepository::new(db.pool().clone())
        .create_team(&team())
        .await
        .unwrap();
    let service = TeamWorkService::new(
        Arc::new(SqliteTeamWorkRepository::new(db.pool().clone())),
        Arc::new(BroadcastEventBus::new(64)),
    );
    for index in 0..11 {
        create_work_task(&service, &format!("capacity-race-{index}"), None, false).await;
    }

    let receipts = join_all((0..11).map(|index| {
        let service = service.clone();
        async move {
            service
                .apply_command(
                    USER_ID,
                    "team-work-test",
                    &format!("capacity-race-{index}"),
                    command(
                        1,
                        &format!("capacity-race-claim-{index}"),
                        TeamWorkActorKind::Agent,
                        &format!("capacity-race-agent-{index}"),
                        TeamWorkCommand::Claim {
                            slot_id: format!("capacity-race-agent-{index}"),
                            agent_backend: format!("capacity-race-profile-{index}"),
                            model: None,
                            lease_duration_ms: 30_000,
                        },
                    ),
                )
                .await
                .unwrap()
        }
    }))
    .await;

    assert_eq!(
        receipts.iter().filter(|receipt| receipt.queue_reason.is_none()).count(),
        10
    );
    assert_eq!(
        receipts
            .iter()
            .filter(|receipt| receipt.queue_reason == Some(aionui_api_types::TeamWorkQueueReason::TeamCapacity))
            .count(),
        1
    );
    let snapshot = service.snapshot(USER_ID, "team-work-test").await.unwrap();
    assert_eq!(snapshot.tasks.iter().filter(|task| task.lease.is_some()).count(), 10);
}
