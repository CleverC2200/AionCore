use aion_agent::tool_policy::ToolPolicy;
use aionui_api_types::{TeamRuntimeSeed, TeamSessionBinding};

use super::team_runtime_tool_policy;

fn team_binding(role: Option<&str>) -> TeamSessionBinding {
    TeamSessionBinding {
        team_id: "team-1".to_string(),
        slot_id: Some("slot-1".to_string()),
        role: role.map(str::to_owned),
        runtime_seed: TeamRuntimeSeed::default(),
        mcp: None,
    }
}

#[test]
fn team_lead_can_only_coordinate_and_inspect() {
    let policy = team_runtime_tool_policy(Some(&team_binding(Some("lead"))));

    assert!(policy.allows("Read"));
    assert!(policy.allows("Grep"));
    assert!(policy.allows("Glob"));
    assert!(policy.allows("team_members"));
    assert!(policy.allows("team_send_message"));
    assert!(policy.allows("team_spawn_agent"));
    assert!(!policy.allows("ExecCommand"));
    assert!(!policy.allows("Write"));
    assert!(!policy.allows("Edit"));
    assert!(!policy.allows("Skill"));
}

#[test]
fn non_leader_sessions_remain_unrestricted() {
    for team in [None, Some(team_binding(Some("teammate"))), Some(team_binding(None))] {
        let policy = team_runtime_tool_policy(team.as_ref());

        assert_eq!(policy, ToolPolicy::Unrestricted);
    }
}
