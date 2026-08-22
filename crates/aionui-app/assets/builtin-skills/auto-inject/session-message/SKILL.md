---
name: session-message
description: Cross-session messaging - deliver a message to another one of this user's conversations, and reply to one you received.
---

# Cross-Session Message Skill

Deliver a message to another one of this user's conversations with the bundled
agent-facing CLI. Delivering is exactly like the user opening that conversation
and pressing send: the recipient starts a turn and decides for itself whether
to reply.

## Rules

1. Use this ONLY when the user selected a target conversation with `@@`, or
   explicitly asked you to communicate with another conversation. Never deliver
   a message on your own initiative.
2. `to` must be a conversation id. Names are not addresses, and there is no
   broadcast — one `send-message` call reaches exactly one conversation.
3. Never pass, inline, export, echo, or set any `AIONUI_...` environment variable.
4. Commands must directly call `"$AIONUI_HELPER_BIN" session ...`. Pass payloads
   through stdin heredocs. Do not write payload JSON files to disk.
5. If the current conversation belongs to a team, do NOT use this skill. Use
   `team send-message` instead.
6. On `rate_limited`, STOP delivering and tell the user. It means the two
   conversations are spinning against each other. Do not retry.
7. Word results precisely. `queued` means "delivered; the other side is busy and
   will see it when it frees up" — it does NOT mean "they received it" or "they
   read it". Never claim a message was read.
8. If the CLI fails, report the failure from stderr/stdout in normal prose. Do
   not claim the message was delivered.

## Reading targets from the user's message

When the user typed `@@`, their message carries a block like:

```
[[AION_SESSIONS]]
v2
{"sessions":[{"name":"重构-鉴权模块","id":"conv_019f…","workspace":"same"},{"name":"文档站改版","id":"conv_01a0…","workspace":"/Users/x/docs（与你不同）"}]}
[[/AION_SESSIONS]]
```

The third line is one JSON object. Read each object in `sessions` and use its
`id`; `name` is display context, not an address.

## Delivering a message

```bash
"$AIONUI_HELPER_BIN" session send-message <<'JSON'
{
  "to": "conv_019f…",
  "message": "接口定完了吗？"
}
JSON
```

## Replying to a message you received

A delivered message arrives with this block at the top:

```
[[AION_SESSION_MESSAGE]]
v2
{"from":{"name":"重构-鉴权模块","id":"conv_019f…"},"workspace":"same","reply_to":"conv_019f…","reply_instruction":"session send-message, to=reply_to"}
[[/AION_SESSION_MESSAGE]]
```

Reply by sending to `reply_to` with the same command:

```bash
"$AIONUI_HELPER_BIN" session send-message <<'JSON'
{
  "to": "conv_019f…",
  "message": "定完了，已经推到 main。"
}
JSON
```

## Legacy v1 read compatibility

Some historical messages contain v1 blocks with no `v2` line. Continue to
read them, but never emit v1. In the syntax below, `{TAB}` means one literal
tab character.

Sender-side v1 target lines are:

```text
name{TAB}id{TAB}workspace: value
```

Use the second tab-separated field as the conversation id. Recipient-side v1
blocks are:

```text
[[AION_SESSION_MESSAGE]]
from: name{TAB}id
workspace: value
reply_to: id{TAB}(reply hint)
[[/AION_SESSION_MESSAGE]]
```

Read the labeled values and use the id after `reply_to:` when replying. This is
read-only compatibility for historical context; all newly generated envelopes
use v2 JSON.

Replying is optional. Decide for yourself whether a reply is useful — there is
no synchronous wait on the other side.

## Finding a target the user only described in prose

```bash
"$AIONUI_HELPER_BIN" session list
```

Optional stdin filters: `q` (name filter), `project_id`, `limit`, `cursor`.

## Cross-workspace rule

When `workspace` is not `same`, the other conversation runs in a different
directory:

- Do NOT use relative paths — they resolve against the recipient's workspace and
  will silently read a different file, or none.
- Do NOT assume the recipient can read your files. Cross-directory access may be
  blocked by its sandbox or permissions.
- To share file content, put the content itself into `message`.

## Getting more detail about a target conversation

For a target's workspace path, whether it is currently running a turn, or
stuck/waiting hints:

```bash
"$AIONUI_HELPER_BIN" diagnose conversations get <<'JSON'
{ "conversation_id": "conv_019f…" }
JSON
```

## Exact schemas

For enum values, error-code meanings, and full field tables:

```bash
"$AIONUI_HELPER_BIN" session capabilities
```
