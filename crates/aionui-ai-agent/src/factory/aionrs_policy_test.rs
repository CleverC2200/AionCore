use aion_agent::tool_policy::ToolPolicy;
use aionui_api_types::{AionrsBuildExtra, TeamRuntimeSeed, TeamSessionBinding};

use super::{TEAM_LEAD_MAX_TURNS, apply_team_runtime_policy};

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
    let mut config = AionrsBuildExtra::default();

    let policy = apply_team_runtime_policy(Some(&team_binding(Some("lead"))), &mut config);

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
    assert_eq!(config.max_turns, Some(TEAM_LEAD_MAX_TURNS));
}

#[test]
fn team_lead_preserves_lower_turn_limit_and_caps_higher_limit() {
    let team = team_binding(Some("lead"));
    let mut lower = AionrsBuildExtra {
        max_turns: Some(8),
        ..Default::default()
    };
    let mut higher = AionrsBuildExtra {
        max_turns: Some(100),
        ..Default::default()
    };

    apply_team_runtime_policy(Some(&team), &mut lower);
    apply_team_runtime_policy(Some(&team), &mut higher);

    assert_eq!(lower.max_turns, Some(8));
    assert_eq!(higher.max_turns, Some(TEAM_LEAD_MAX_TURNS));
}

#[test]
fn non_leader_sessions_remain_unrestricted() {
    for team in [None, Some(team_binding(Some("teammate"))), Some(team_binding(None))] {
        let mut config = AionrsBuildExtra {
            max_turns: None,
            ..Default::default()
        };

        let policy = apply_team_runtime_policy(team.as_ref(), &mut config);

        assert_eq!(policy, ToolPolicy::Unrestricted);
        assert_eq!(config.max_turns, None);
    }
}
