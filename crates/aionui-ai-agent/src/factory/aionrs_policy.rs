use aion_agent::tool_policy::ToolPolicy;
use aionui_api_types::TeamSessionBinding;
use aionui_team_prompts::visible_team_tool_descriptors;
use tracing::info;

const TEAM_LEAD_WORKSPACE_TOOLS: [&str; 5] = ["Read", "Grep", "Glob", "Edit", "Write"];

pub(super) fn team_runtime_tool_policy(team: Option<&TeamSessionBinding>) -> ToolPolicy {
    let Some(binding) = team.filter(|binding| {
        binding
            .role
            .as_deref()
            .is_some_and(|role| role.eq_ignore_ascii_case("lead") || role.eq_ignore_ascii_case("leader"))
    }) else {
        return ToolPolicy::Unrestricted;
    };

    info!(
        target: "aionui_feedback_diagnostics",
        event = "feedback.team.lead_runtime_policy_applied",
        team_id = %binding.team_id,
        slot_id = binding.slot_id.as_deref().unwrap_or("none"),
        "Applied Team Leader coordination-first runtime policy"
    );

    let tool_names = TEAM_LEAD_WORKSPACE_TOOLS
        .into_iter()
        .map(str::to_owned)
        .chain(visible_team_tool_descriptors(true).into_iter().map(|tool| tool.name));
    ToolPolicy::allow_only(tool_names)
}

#[cfg(test)]
#[path = "aionrs_policy_test.rs"]
mod aionrs_policy_test;
