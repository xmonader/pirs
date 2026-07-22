//! Host-owned control-pin channel for `<system-reminder> kind=…` messages.
//!
//! Extension packs pin plan text and inject stop_gate / verify / thrash nudges
//! as user messages. A pack that strips every system-reminder (or rewrites
//! context carelessly) can erase sibling control kinds. After any
//! `transform_context` rewrite the agent loop re-injects **protected** kinds
//! that were present before the rewrite but missing afterward.
//!
//! Plan / goal pins are *not* protected here — packs own de-dupe for those
//! (replace only their own kind). Protected kinds are one-shot control pressure
//! that must remain model-visible once injected.

use pirs_ai::{ContentBlock, Message, UserContent};

/// Control kinds the host will restore if a context transform drops them.
pub const PROTECTED_KINDS: &[&str] = &[
    "stop_gate",
    "verify",
    "edit_fail",
    "repeat",
    "no_progress",
];

/// Wrap a body in the standard reminder envelope.
pub fn wrap_reminder(kind: &str, body: &str) -> String {
    format!("<system-reminder> kind={kind}\n{body}\n</system-reminder>")
}

/// Extract `kind` from a `<system-reminder> kind=…` message body, if present.
pub fn reminder_kind(text: &str) -> Option<&str> {
    if !text.contains("<system-reminder>") {
        return None;
    }
    let marker = "kind=";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '>' || c == '\n' || c == '\r')
        .unwrap_or(rest.len());
    let kind = rest[..end].trim();
    if kind.is_empty() {
        None
    } else {
        Some(kind)
    }
}

fn user_text(m: &Message) -> Option<&str> {
    match m {
        Message::User(u) => match &u.content {
            UserContent::Text(t) => Some(t.as_str()),
            UserContent::Blocks(blocks) => blocks.first().and_then(|b| b.as_text()),
        },
        _ => None,
    }
}

/// Kind of a user control-pin message, if any.
pub fn message_reminder_kind(m: &Message) -> Option<&str> {
    user_text(m).and_then(reminder_kind)
}

/// True when this user message is a system-reminder of the given kind.
pub fn is_reminder_kind(m: &Message, kind: &str) -> bool {
    message_reminder_kind(m) == Some(kind)
}

/// Drop only user messages whose reminder kind matches `kind`. Other
/// system-reminders and all non-user messages are kept.
pub fn strip_reminder_kind(messages: Vec<Message>, kind: &str) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|m| !is_reminder_kind(m, kind))
        .collect()
}

/// After a pack rewrites context, re-insert protected control pins that the
/// rewrite removed. Inserts each missing kind once (most recent from `before`)
/// near the tail so they stay model-visible without becoming free-form spam.
pub fn preserve_control_pins(before: &[Message], mut after: Vec<Message>) -> Vec<Message> {
    for kind in PROTECTED_KINDS {
        let still_present = after.iter().any(|m| is_reminder_kind(m, kind));
        if still_present {
            continue;
        }
        let Some(original) = before
            .iter()
            .rev()
            .find(|m| is_reminder_kind(m, kind))
            .cloned()
        else {
            continue;
        };
        // Prefer sitting just before the last message (same convention as plan pins).
        let idx = after.len().saturating_sub(1);
        after.insert(idx, original);
    }
    after
}

/// Restore tool_use / tool_result adjacency after context transforms.
///
/// The wire protocol -- Anthropic Messages and OpenAI chat-completions alike --
/// requires every tool result to sit in the turn immediately following the
/// assistant `tool_use`/`tool_calls` that issued it. But the plan/control pins
/// above (and rhai `on_context` packs like conductor / weak-model) insert a user
/// message at `len-1`. When the tail of the conversation is a trailing assistant
/// tool_use followed by its tool_result, that `len-1` insertion lands the pin
/// *between* them. Serialized, the tool_result then falls into a separate,
/// non-adjacent user block and the backend rejects the whole request
/// ("an assistant message with 'tool_calls' must be followed by tool messages
/// ... did not have response messages"). Anthropic-format gateways (e.g. Kimi's
/// coding endpoint) enforce this strictly.
///
/// This moves any non-result message that was inserted into an assistant's
/// trailing tool_result run to *after* that run, restoring adjacency no matter
/// which pack (bundled or user-authored) caused it. It only reorders within a
/// single assistant response group, never across turns, and preserves relative
/// order within the results and within the displaced messages.
pub fn enforce_tool_result_adjacency(messages: &mut Vec<Message>) {
    fn is_tool_use_assistant(m: &Message) -> bool {
        matches!(m, Message::Assistant(a)
            if a.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. })))
    }
    let mut i = 0;
    while i < messages.len() {
        if !is_tool_use_assistant(&messages[i]) {
            i += 1;
            continue;
        }
        // The response group is everything up to the next assistant message.
        let start = i + 1;
        let mut end = start;
        while end < messages.len() && !matches!(messages[end], Message::Assistant(_)) {
            end += 1;
        }
        let has_result = messages[start..end]
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)));
        let has_other = messages[start..end]
            .iter()
            .any(|m| !matches!(m, Message::ToolResult(_)));
        // Only touch a group that actually interleaves results with non-results.
        if has_result && has_other {
            let (results, others): (Vec<Message>, Vec<Message>) = messages[start..end]
                .to_vec()
                .into_iter()
                .partition(|m| matches!(m, Message::ToolResult(_)));
            let reordered: Vec<Message> = results.into_iter().chain(others).collect();
            messages.splice(start..end, reordered);
        }
        i = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::Message;

    fn user(s: &str) -> Message {
        Message::user(s)
    }

    fn assistant_tool_use(id: &str) -> Message {
        Message::Assistant(pirs_ai::AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: id.into(),
                name: "update_plan".into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            }],
            ..Default::default()
        })
    }

    fn tool_result(id: &str) -> Message {
        Message::ToolResult(pirs_ai::ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "update_plan".into(),
            content: vec![ContentBlock::text("blocked")],
            details: None,
            is_error: true,
            terminate: false,
            timestamp: 0,
        })
    }

    fn is_tool_result(m: &Message) -> bool {
        matches!(m, Message::ToolResult(_))
    }

    #[test]
    fn adjacency_moves_pin_after_tool_result() {
        // The exact corruption: a pin lands between a trailing tool_use and its
        // tool_result. Repair must put the result adjacent to the call.
        let mut msgs = vec![
            user("do the thing"),
            assistant_tool_use("update_plan:0"),
            user(&wrap_reminder("plan", "PLAN: step 1")),
            tool_result("update_plan:0"),
        ];
        enforce_tool_result_adjacency(&mut msgs);
        assert!(
            is_tool_result(&msgs[2]),
            "tool_result must immediately follow the tool_use"
        );
        assert!(
            is_reminder_kind(&msgs[3], "plan"),
            "the pin must be displaced to after the tool_result"
        );
        assert_eq!(msgs.len(), 4, "no messages added or dropped");
    }

    #[test]
    fn adjacency_is_noop_when_result_already_adjacent() {
        // Message has no PartialEq, so assert on structure/order instead of ==.
        let mut msgs = vec![
            assistant_tool_use("t1"),
            tool_result("t1"),
            user(&wrap_reminder("plan", "PLAN")),
        ];
        enforce_tool_result_adjacency(&mut msgs);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0], Message::Assistant(_)));
        assert!(is_tool_result(&msgs[1]));
        assert!(is_reminder_kind(&msgs[2], "plan"));
    }

    #[test]
    fn adjacency_keeps_multiple_results_together_and_pin_last() {
        let mut msgs = vec![
            assistant_tool_use("t1"),
            tool_result("t1"),
            user(&wrap_reminder("plan", "PLAN")),
            tool_result("t2"),
        ];
        enforce_tool_result_adjacency(&mut msgs);
        assert!(is_tool_result(&msgs[1]) && is_tool_result(&msgs[2]));
        assert!(is_reminder_kind(&msgs[3], "plan"));
    }

    #[test]
    fn adjacency_does_not_cross_turn_boundaries() {
        // A pin before an unrelated later assistant turn stays where it is.
        let mut msgs = vec![
            assistant_tool_use("t1"),
            tool_result("t1"),
            Message::Assistant(pirs_ai::AssistantMessage {
                content: vec![ContentBlock::text("done")],
                ..Default::default()
            }),
            user(&wrap_reminder("plan", "PLAN")),
        ];
        enforce_tool_result_adjacency(&mut msgs);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(msgs[0], Message::Assistant(_)));
        assert!(is_tool_result(&msgs[1]));
        assert!(matches!(msgs[2], Message::Assistant(_)));
        assert!(is_reminder_kind(&msgs[3], "plan"), "pin stays in its own turn");
    }

    #[test]
    fn reminder_kind_parses_standard_envelope() {
        let t = wrap_reminder("stop_gate", "STOP GATE: run tests");
        assert_eq!(reminder_kind(&t), Some("stop_gate"));
        assert_eq!(
            reminder_kind("<system-reminder> kind=plan\nx\n</system-reminder>"),
            Some("plan")
        );
        assert_eq!(reminder_kind("plain user text"), None);
    }

    #[test]
    fn strip_reminder_kind_only_drops_matching_kind() {
        let msgs = vec![
            user("hello"),
            user(&wrap_reminder("plan", "do x")),
            user(&wrap_reminder("stop_gate", "STOP GATE")),
            user(&wrap_reminder("verify", "run tests")),
        ];
        let out = strip_reminder_kind(msgs, "plan");
        assert_eq!(out.len(), 3);
        assert!(out.iter().any(|m| is_reminder_kind(m, "stop_gate")));
        assert!(out.iter().any(|m| is_reminder_kind(m, "verify")));
        assert!(!out.iter().any(|m| is_reminder_kind(m, "plan")));
    }

    #[test]
    fn preserve_restores_stop_gate_when_pack_strips_all_reminders() {
        // Simulate the pre-fix weak-model on_context bug: strip every
        // system-reminder, re-append only plan.
        let before = vec![
            user("task"),
            user(&wrap_reminder("plan", "1. edit")),
            user(&wrap_reminder(
                "stop_gate",
                "STOP GATE: you edited files but have not shown tests",
            )),
            user("all done"),
        ];
        let after_bad: Vec<Message> = before
            .iter()
            .filter(|m| {
                user_text(m)
                    .map(|t| !t.contains("<system-reminder>"))
                    .unwrap_or(true)
            })
            .cloned()
            .chain(std::iter::once(user(&wrap_reminder("plan", "1. edit"))))
            .collect();
        assert!(
            !after_bad.iter().any(|m| is_reminder_kind(m, "stop_gate")),
            "precondition: bad transform dropped stop_gate"
        );

        let restored = preserve_control_pins(&before, after_bad);
        assert!(
            restored.iter().any(|m| is_reminder_kind(m, "stop_gate")),
            "host must restore stop_gate: {restored:?}"
        );
        assert!(
            restored.iter().any(|m| is_reminder_kind(m, "plan")),
            "plan pin should remain"
        );
        // Exactly one stop_gate (not unbounded accumulation).
        let gates = restored
            .iter()
            .filter(|m| is_reminder_kind(m, "stop_gate"))
            .count();
        assert_eq!(gates, 1);
    }

    #[test]
    fn preserve_does_not_duplicate_when_still_present() {
        let gate = user(&wrap_reminder("stop_gate", "STOP GATE"));
        let before = vec![user("task"), gate.clone()];
        let after = vec![user("task"), gate];
        let out = preserve_control_pins(&before, after);
        assert_eq!(
            out.iter()
                .filter(|m| is_reminder_kind(m, "stop_gate"))
                .count(),
            1
        );
    }

    #[test]
    fn preserve_restores_verify_and_thrash_kinds() {
        let before = vec![
            user(&wrap_reminder("verify", "run build")),
            user(&wrap_reminder("edit_fail", "re-read")),
            user(&wrap_reminder("repeat", "different approach")),
            user(&wrap_reminder("no_progress", "one step")),
            user("done"),
        ];
        let after = vec![user("done")];
        let out = preserve_control_pins(&before, after);
        for kind in ["verify", "edit_fail", "repeat", "no_progress"] {
            assert!(
                out.iter().any(|m| is_reminder_kind(m, kind)),
                "missing restored kind={kind} in {out:?}"
            );
        }
    }

    #[test]
    fn plan_is_not_auto_restored() {
        // Packs own plan de-dupe; host must not resurrect an old plan pin.
        let before = vec![user(&wrap_reminder("plan", "old")), user("hi")];
        let after = vec![user("hi")];
        let out = preserve_control_pins(&before, after);
        assert!(!out.iter().any(|m| is_reminder_kind(m, "plan")));
    }
}
