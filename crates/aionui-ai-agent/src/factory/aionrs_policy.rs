use aion_agent::tool_policy::ToolPolicy;
use aionui_api_types::{AionrsBuildExtra, TeamSessionBinding};
use aionui_team_prompts::visible_team_tool_descriptors;
use tracing::info;

const TEAM_LEAD_MAX_TURNS: usize = 20;
const TEAM_LEAD_READ_ONLY_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

pub(super) fn apply_team_runtime_policy(
    team: Option<&TeamSessionBinding>,
    config: &mut AionrsBuildExtra,
) -> ToolPolicy {
    let Some(binding) = team.filter(|binding| {
        binding
            .role
            .as_deref()
            .is_some_and(|role| role.eq_ignore_ascii_case("lead"))
    }) else {
        return ToolPolicy::Unrestricted;
    };

    config.max_turns = Some(match config.max_turns {
        Some(limit) if limit > 0 => limit.min(TEAM_LEAD_MAX_TURNS),
        _ => TEAM_LEAD_MAX_TURNS,
    });
    info!(
        target: "aionui_feedback_diagnostics",
        event = "feedback.team.lead_runtime_policy_applied",
        team_id = %binding.team_id,
        slot_id = binding.slot_id.as_deref().unwrap_or("none"),
        max_turns = config.max_turns.unwrap_or(TEAM_LEAD_MAX_TURNS),
        "Applied Team Leader coordination-only runtime policy"
    );

    let tool_names = TEAM_LEAD_READ_ONLY_TOOLS
        .into_iter()
        .map(str::to_owned)
        .chain(visible_team_tool_descriptors(true).into_iter().map(|tool| tool.name));
    ToolPolicy::allow_only(tool_names)
}

#[cfg(test)]
#[path = "aionrs_policy_test.rs"]
mod aionrs_policy_test;
