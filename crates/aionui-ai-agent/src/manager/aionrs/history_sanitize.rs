//! Sanitize a resumed aionrs session's message history before it is replayed
//! to a provider.
//!
//! Background: when the user clicks "Stop" on a tool-call mid-stream, aionrs
//! may persist an assistant message that contains `ToolUse` content blocks
//! but whose tool calls were never followed up by the matching `ToolResult`
//! blocks. On the next turn, the engine replays history verbatim and strict
//! providers reject the request:
//!   - Ollama-compatible providers (e.g. `qwen3:8b`) return
//!     `400 invalid message content type: <nil>` because the assistant
//!     message has `tool_calls != null` but `content == null`.
//!   - Some OpenAI-compatible proxies (e.g. DeepSeek behind a strict gateway)
//!     return `400 invalid_request_error` for the same reason.
//!
//! Fix: drop assistant messages that
//!   1. contain at least one `ToolUse` block,
//!   2. have NO non-empty `Text` content, AND
//!   3. have NO subsequent `ToolResult` block (in any later message) that
//!      references one of those tool-use ids.
//!
//! Also strip malformed tool calls whose `name` is empty, plus their matching
//! results. Those are not valid protocol tool calls and strict providers reject
//! them even when a matching result is present.
//!
//! A complete `assistant(tool_use) → user(tool_result)` pair is left intact —
//! that shape is valid and required by every provider.
//!
//! This logic is intentionally a free function (not a method on
//! `AionrsAgentManager`) so it can be unit-tested in isolation and so we do
//! not add yet another field to a manager (per `AGENTS.md`).

use std::collections::HashSet;

use aion_types::message::{ContentBlock, Message, Role};

/// Drop orphaned assistant tool-call messages from a session's history.
///
/// Returns the number of messages removed.
///
/// Operates in-place on `messages`. Safe to call on an empty vector.
pub fn sanitize_session_messages(messages: &mut Vec<Message>) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut removed = strip_malformed_tool_calls(messages);

    // Collect every tool_use_id that has a matching tool_result anywhere
    // in the entire history. We do this in one pass so that the lookup
    // for each candidate assistant message is O(1).
    let mut answered_tool_use_ids: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        for block in &msg.content {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                answered_tool_use_ids.insert(tool_use_id.clone());
            }
        }
    }

    let original_len = messages.len();
    messages.retain(|msg| !is_orphaned_assistant_tool_call(msg, &answered_tool_use_ids));
    removed += original_len - messages.len();
    removed
}

/// Drop cached negative ToolSearch observations before reconnecting MCP tools.
///
/// Tool availability is runtime state. A resumed conversation can otherwise
/// replay an old `No deferred tools matching ... found.` result and keep
/// refusing a tool that the freshly connected registry now advertises. User
/// requests and unrelated assistant answers are preserved.
pub fn sanitize_stale_tool_discovery_messages(messages: &mut Vec<Message>) -> usize {
    const MISS_PREFIX: &str = "No deferred tools matching \"";
    const MISS_SUFFIX: &str = "\" found.";

    let mut stale_tool_use_ids = HashSet::new();
    let mut stale_tool_names = HashSet::new();

    for message in messages.iter() {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_use_id, content, ..
            } = block
                && let Some(tool_name) = content
                    .strip_prefix(MISS_PREFIX)
                    .and_then(|value| value.strip_suffix(MISS_SUFFIX))
                && !tool_name.trim().is_empty()
            {
                stale_tool_use_ids.insert(tool_use_id.clone());
                stale_tool_names.insert(tool_name.trim().to_lowercase());
            }
        }
    }

    if stale_tool_use_ids.is_empty() {
        return 0;
    }

    let original_len = messages.len();
    messages.retain(|message| {
        let belongs_to_stale_search = message.content.iter().any(|block| match block {
            ContentBlock::ToolUse { id, .. } => stale_tool_use_ids.contains(id),
            ContentBlock::ToolResult { tool_use_id, .. } => stale_tool_use_ids.contains(tool_use_id),
            _ => false,
        });
        if belongs_to_stale_search {
            return false;
        }

        if message.role != Role::Assistant {
            return true;
        }

        let replay_text = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let mentions_stale_tool = stale_tool_names.iter().any(|name| replay_text.contains(name));
        let repeats_stale_miss = [
            "not available",
            "doesn't exist",
            "does not exist",
            "not loaded",
            "no schema",
            "不存在",
            "未加载",
            "无 schema",
        ]
        .iter()
        .any(|marker| replay_text.contains(marker));

        !(mentions_stale_tool && repeats_stale_miss)
    });

    original_len - messages.len()
}

fn strip_malformed_tool_calls(messages: &mut Vec<Message>) -> usize {
    let malformed_tool_use_ids: HashSet<String> = messages
        .iter()
        .flat_map(|msg| msg.content.iter())
        .filter_map(|block| {
            if let ContentBlock::ToolUse { id, name, .. } = block
                && name.trim().is_empty()
            {
                return Some(id.clone());
            }
            None
        })
        .collect();

    if malformed_tool_use_ids.is_empty() {
        return 0;
    }

    for msg in messages.iter_mut() {
        msg.content.retain(|block| match block {
            ContentBlock::ToolUse { name, .. } => !name.trim().is_empty(),
            ContentBlock::ToolResult { tool_use_id, .. } => !malformed_tool_use_ids.contains(tool_use_id),
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::ProviderItem { .. } => true,
        });
    }

    let original_len = messages.len();
    messages.retain(|msg| !msg.content.is_empty());
    original_len - messages.len()
}

/// True iff `msg` is an assistant message that has tool_use blocks, no
/// non-empty text, and at least one of its tool_use ids has no matching
/// tool_result anywhere in the history.
fn is_orphaned_assistant_tool_call(msg: &Message, answered: &HashSet<String>) -> bool {
    if msg.role != Role::Assistant {
        return false;
    }

    let mut has_tool_use = false;
    let mut has_unanswered = false;
    let mut has_text = false;

    for block in &msg.content {
        match block {
            ContentBlock::ToolUse { id, .. } => {
                has_tool_use = true;
                if !answered.contains(id) {
                    has_unanswered = true;
                }
            }
            ContentBlock::Text { text } => {
                if !text.trim().is_empty() {
                    has_text = true;
                }
            }
            // Thinking, provider-owned items, ToolResult, and Image blocks do
            // not change the orphan determination. ToolResult should not
            // appear on assistant messages, but if it does we ignore it here.
            ContentBlock::Thinking { .. }
            | ContentBlock::ProviderItem { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Image { .. } => {}
        }
    }

    has_tool_use && has_unanswered && !has_text
}

#[cfg(test)]
#[path = "history_sanitize_test.rs"]
mod history_sanitize_test;
