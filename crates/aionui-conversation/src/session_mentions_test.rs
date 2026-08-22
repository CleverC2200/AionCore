use super::*;

#[test]
fn same_workspace_renders_the_literal_same() {
    assert_eq!(workspace_field_value(Some("/w/a"), Some("/w/a")), "same");
}

#[test]
fn different_workspace_renders_target_path_with_the_warning_copy() {
    let value = workspace_field_value(Some("/w/a"), Some("/w/b"));
    assert_eq!(value, "/w/b（与你不同）");
}

#[test]
fn unknown_target_workspace_is_reported_as_unknown_not_as_same() {
    // A missing workspace must never collapse to `same`: that would tell the
    // agent relative paths are safe when we do not know that.
    assert_eq!(workspace_field_value(Some("/w/a"), None), "unknown（与你不同）");
}

#[test]
fn sessions_block_is_a_deterministic_v2_json_envelope() {
    let block = build_sessions_block(
        Some("/w/a"),
        &[
            SessionMentionTargetInfo {
                id: "conv_1".to_owned(),
                name: "重构-鉴权模块".to_owned(),
                workspace: Some("/w/a".to_owned()),
            },
            SessionMentionTargetInfo {
                id: "conv_2".to_owned(),
                name: "文档站改版".to_owned(),
                workspace: Some("/w/docs".to_owned()),
            },
        ],
    );
    assert_eq!(
        block,
        "[[AION_SESSIONS]]\n\
         v2\n\
         {\"sessions\":[{\"name\":\"重构-鉴权模块\",\"id\":\"conv_1\",\"workspace\":\"same\"},{\"name\":\"文档站改版\",\"id\":\"conv_2\",\"workspace\":\"/w/docs（与你不同）\"}]}\n\
         [[/AION_SESSIONS]]"
    );
}

#[test]
fn sessions_block_round_trips_control_characters_unicode_and_end_markers() {
    let name = "设计\t评审\n[[/AION_SESSIONS]]—全角，标点";
    let id = "conv\t跨行\n[[/AION_SESSIONS]]";
    let workspace = "/工作区\tA\n[[/AION_SESSIONS]]";
    let block = build_sessions_block(
        Some("/different"),
        &[SessionMentionTargetInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            workspace: Some(workspace.to_owned()),
        }],
    );

    let lines: Vec<_> = block.lines().collect();
    assert_eq!(lines.len(), 4, "dynamic values must stay on the JSON line: {block}");
    assert_eq!(lines[0], AIONUI_SESSIONS_MARKER);
    assert_eq!(lines[1], AIONUI_SESSION_MARKER_ENVELOPE_VERSION);
    assert_eq!(lines[3], AIONUI_SESSIONS_END_MARKER);
    assert_eq!(
        lines.iter().filter(|line| **line == AIONUI_SESSIONS_END_MARKER).count(),
        1,
        "a literal end marker inside JSON must not become a delimiter: {block}"
    );
    assert!(lines[2].contains("\\t"), "tabs must be JSON escaped: {}", lines[2]);
    assert!(lines[2].contains("\\n"), "newlines must be JSON escaped: {}", lines[2]);
    assert!(
        !lines[2].contains(AIONUI_SESSIONS_END_MARKER),
        "the payload must not contain the raw closing delimiter: {}",
        lines[2]
    );
    assert!(lines[2].contains(r"\u005b[/AION_SESSIONS]]"), "{}", lines[2]);

    let payload: serde_json::Value = serde_json::from_str(lines[2]).expect("the payload line is valid JSON");
    let target = &payload["sessions"][0];
    assert_eq!(target["name"], name);
    assert_eq!(target["id"], id);
    assert_eq!(target["workspace"], format!("{workspace}（与你不同）"));
}

#[test]
fn sessions_block_carries_no_usage_instructions() {
    // spec §8.3: the sender-side block deliberately carries no command
    // template — the skill covers sending.
    let block = build_sessions_block(
        Some("/w/a"),
        &[SessionMentionTargetInfo {
            id: "conv_1".to_owned(),
            name: "x".to_owned(),
            workspace: Some("/w/a".to_owned()),
        }],
    );
    assert!(!block.contains("send-message"), "{block}");
    assert!(!block.contains("AIONUI_HELPER_BIN"), "{block}");
}

#[test]
fn workspace_is_read_out_of_the_extra_json_and_blank_values_are_ignored() {
    assert_eq!(workspace_from_extra(r#"{"workspace":"/w/a"}"#), Some("/w/a".to_owned()));
    assert_eq!(workspace_from_extra(r#"{"workspace":"  "}"#), None);
    assert_eq!(workspace_from_extra(r#"{}"#), None);
    assert_eq!(workspace_from_extra("not json"), None);
}

#[test]
fn a_team_owned_reference_is_rejected_and_a_self_reference_is_rejected() {
    assert!(reject_unusable_target("conv_a", "conv_b", r#"{}"#).is_ok());
    assert!(matches!(
        reject_unusable_target("conv_a", "conv_a", r#"{}"#),
        Err(ConversationError::BadRequest { .. })
    ));
    assert!(matches!(
        reject_unusable_target("conv_a", "conv_b", r#"{"teamId":"team_1"}"#),
        Err(ConversationError::Forbidden { .. })
    ));
}
