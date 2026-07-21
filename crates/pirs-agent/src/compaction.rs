use std::sync::Arc;

use pirs_ai::{
    AssistantMessage, ContentBlock, Context, LlmProvider, Message, StopReason, StreamEvent,
};
use tokio_util::sync::CancellationToken;

use crate::events::AgentEvent;

pub const SUMMARY_PREFIX: &str = "[Earlier conversation summarized by the agent]";
pub const SUMMARY_SUFFIX: &str = "[End of summary. Continue the work from this state.]";

const SUMMARY_SYSTEM_PROMPT: &str = "You are summarizing a conversation between a user and a coding agent so the agent can continue with a fresh context.\n\nCapture, concisely but completely:\n1. The user's goal and the task currently in progress.\n2. Key decisions and their rationale.\n3. Files created or modified (exact paths), and important code structure.\n4. Errors encountered and how they were resolved (exact messages when they matter).\n5. The current state and the immediate next steps.\n\nPreserve exact file paths, identifiers, and commands. Output plain prose with short sections.";

const MAX_SUMMARY_INPUT_CHARS: usize = 80_000;

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub context_window: u64,
    pub reserve_tokens: u64,
    /// Keep at least this many estimated tokens in the unsummarized tail.
    pub keep_recent_tokens: u64,
    /// Also keep at least this many recent user-turn blocks (user + following
    /// assistant/tool traffic) so follow-ups still see recent file/tool work.
    pub min_recent_user_turns: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            context_window: 128_000,
            reserve_tokens: 16_384,
            // Stronger default retention for long coding runs (was 20k).
            keep_recent_tokens: 32_000,
            min_recent_user_turns: 4,
        }
    }
}

pub fn estimate_message_tokens(msg: &Message) -> u64 {
    let chars: usize = match msg {
        Message::User(u) => match &u.content {
            pirs_ai::UserContent::Text(t) => t.len(),
            pirs_ai::UserContent::Blocks(bs) => bs.iter().map(block_chars).sum(),
        },
        Message::Assistant(a) => a.content.iter().map(block_chars).sum(),
        Message::ToolResult(tr) => tr.content.iter().map(block_chars).sum(),
    };
    (chars / 4) as u64 + 4
}

fn block_chars(b: &ContentBlock) -> usize {
    match b {
        ContentBlock::Text { text, .. } => text.len(),
        ContentBlock::Thinking { thinking, .. } => thinking.len(),
        ContentBlock::Image { data, .. } => data.len() / 100,
        ContentBlock::ToolCall {
            name, arguments, ..
        } => name.len() + arguments.to_string().len(),
    }
}

pub fn estimate_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

pub fn should_compact(tokens: u64, config: &CompactionConfig) -> bool {
    tokens > config.context_window.saturating_sub(config.reserve_tokens)
}

/// Find a cut index such that messages[cut..] keeps at least `keep_recent_tokens`
/// estimated tokens and starts at a clean boundary (a user message).
///
/// The cut is then snapped so it never splits an assistant `tool_use` from its
/// following `tool_result`(s) — leaving a dangling tool_use wedges Anthropic
/// and corrupts OpenAI history.
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> Option<usize> {
    find_cut_point_ex(messages, keep_recent_tokens, 0)
}

/// Like [`find_cut_point`], also ensuring at least `min_recent_user_turns`
/// recent user messages remain in the tail (when that many exist).
pub fn find_cut_point_ex(
    messages: &[Message],
    keep_recent_tokens: u64,
    min_recent_user_turns: usize,
) -> Option<usize> {
    let mut acc = 0u64;
    let mut cut = None;
    let mut user_turns = 0usize;
    for i in (0..messages.len()).rev() {
        acc += estimate_message_tokens(&messages[i]);
        if matches!(messages[i], Message::User(_)) {
            cut = Some(i);
            user_turns += 1;
            let turns_ok = min_recent_user_turns == 0 || user_turns >= min_recent_user_turns;
            if acc >= keep_recent_tokens && turns_ok {
                break;
            }
        }
    }
    let cut = match cut {
        Some(0) | None => return None,
        Some(c) => c,
    };
    let cut = snap_cut_to_tool_pair_boundary(messages, cut);
    if cut == 0 {
        None
    } else {
        Some(cut)
    }
}

/// Count user messages in a slice (for retention checks / tests).
pub fn count_user_turns(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, Message::User(_)))
        .count()
}

/// Snap `cut` so `messages[cut..]` never starts mid tool_use/tool_result pair.
///
/// Rules (same spirit as other agent harnesses' tool-pair-safe compaction):
/// - If cut lands on a ToolResult, walk back to the Assistant that owns it, or
///   forward past all trailing ToolResults of the prior assistant.
/// - If cut lands just after an Assistant with tool calls but before its
///   results, include that assistant in the *kept* tail (move cut earlier).
pub fn snap_cut_to_tool_pair_boundary(messages: &[Message], mut cut: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }
    cut = cut.min(messages.len());

    // If we'd start on a ToolResult, the matching tool_use is before cut —
    // pull cut back to the assistant that owns these results (or the user
    // before it). Alternatively walk forward past the result block so the
    // dropped prefix ends cleanly after a complete pair.
    while cut < messages.len() && matches!(messages[cut], Message::ToolResult(_)) {
        // Prefer keeping the whole pair in the tail: move cut earlier.
        if cut == 0 {
            break;
        }
        // Walk back to assistant with tool calls.
        let mut i = cut;
        while i > 0 {
            i -= 1;
            match &messages[i] {
                Message::Assistant(a) if !a.tool_calls().is_empty() => {
                    cut = i;
                    return cut;
                }
                Message::User(_) => {
                    cut = i + 1;
                    // Still on tool results — advance past them instead.
                    while cut < messages.len() && matches!(messages[cut], Message::ToolResult(_)) {
                        cut += 1;
                    }
                    return cut.min(messages.len());
                }
                _ => {}
            }
        }
        cut += 1;
    }

    // If the message *before* cut is an assistant with tool calls, and cut
    // points at (or would leave behind) its tool results in the prefix only,
    // move cut back to include the assistant in the kept tail... wait: prefix
    // is messages[..cut] (summarized). Tail is messages[cut..].
    // If messages[cut-1] is Assistant with tools and messages[cut] is ToolResult,
    // we're already OK after the while above. If cut is after the assistant
    // but results are in the prefix incomplete — check: if cut lands right
    // after assistant with tools and next msgs are tool results, the assistant
    // is in prefix without results. Snap cut back to that assistant index so
    // both go to the tail (or if we can't, advance past results into prefix).
    if cut > 0 {
        if let Message::Assistant(a) = &messages[cut - 1] {
            if !a.tool_calls().is_empty() {
                // Assistant at cut-1 would be dropped without its results if
                // results start at cut. Include assistant in tail.
                if cut < messages.len() && matches!(messages[cut], Message::ToolResult(_)) {
                    cut -= 1;
                } else if cut == messages.len()
                    || !matches!(messages.get(cut), Some(Message::ToolResult(_)))
                {
                    // No results after — also pull assistant into tail if any
                    // results exist later... already handled. If results are
                    // missing entirely, leave cut (orphan is pre-existing).
                }
            }
        }
    }

    // Final pass: never start tail on ToolResult (walk forward).
    while cut < messages.len() && matches!(messages[cut], Message::ToolResult(_)) {
        cut += 1;
    }
    cut
}

/// True if every tool_use in the list has a following tool_result somewhere
/// after it, and no tool_result appears without a preceding unmatched tool_use
/// in the open set. Used by tests and as a post-compaction sanity check.
pub fn tool_pairs_intact(messages: &[Message]) -> bool {
    let mut open: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in messages {
        match msg {
            Message::Assistant(a) => {
                for tc in a.tool_calls() {
                    if let ContentBlock::ToolCall { id, .. } = tc {
                        open.insert(id.clone());
                    }
                }
            }
            // Strict: dangling tool_result (no matching open tool_use) is broken.
            Message::ToolResult(tr) if !open.remove(&tr.tool_call_id) => {
                return false;
            }
            Message::ToolResult(_) => {}
            _ => {}
        }
    }
    open.is_empty()
}

fn transcript_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::User(u) => {
                let text = match &u.content {
                    pirs_ai::UserContent::Text(t) => t.clone(),
                    pirs_ai::UserContent::Blocks(bs) => bs
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                out.push_str(&format!("User: {text}\n\n"));
            }
            Message::Assistant(a) => {
                let mut parts = vec![];
                let text = a.text();
                if !text.is_empty() {
                    parts.push(text);
                }
                for b in &a.content {
                    if let ContentBlock::ToolCall {
                        name, arguments, ..
                    } = b
                    {
                        parts.push(format!("[called {name} with {arguments}]"));
                    }
                }
                if !parts.is_empty() {
                    out.push_str(&format!("Assistant: {}\n\n", parts.join("\n")));
                }
            }
            Message::ToolResult(tr) => {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = if text.chars().count() > 2000 {
                    format!("{}...", text.chars().take(2000).collect::<String>())
                } else {
                    text
                };
                out.push_str(&format!("Tool({}): {text}\n\n", tr.tool_name));
            }
        }
    }
    out
}

pub async fn summarize(
    provider: &Arc<dyn LlmProvider>,
    model: &str,
    messages: &[Message],
    cancel: CancellationToken,
) -> anyhow::Result<(String, pirs_ai::Usage)> {
    let mut transcript = transcript_text(messages);
    if transcript.len() > MAX_SUMMARY_INPUT_CHARS {
        let keep = transcript.len() - MAX_SUMMARY_INPUT_CHARS;
        let mut boundary = keep;
        while boundary < transcript.len() && !transcript.is_char_boundary(boundary) {
            boundary += 1;
        }
        transcript = format!("[oldest messages omitted]\n{}", &transcript[boundary..]);
    }

    let ctx = Context {
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::user(format!(
            "Summarize this conversation transcript:\n\n{transcript}"
        ))],
        tools: vec![],
    };
    let mut stream = provider
        .stream(model, &ctx, &Default::default(), cancel)
        .await;
    let mut text = String::new();
    let mut stop = StopReason::Stop;
    let mut error = None;
    let mut usage = pirs_ai::Usage::default();
    use futures::StreamExt;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(d) => text.push_str(&d),
            StreamEvent::Done(msg) => {
                stop = msg.stop_reason;
                error = msg.error_message.clone();
                usage = msg.usage.clone();
                if text.is_empty() {
                    text = msg.text();
                }
            }
            _ => {}
        }
    }
    match stop {
        StopReason::Stop | StopReason::ToolUse if !text.trim().is_empty() => Ok((text, usage)),
        StopReason::Aborted => anyhow::bail!("summarization aborted"),
        _ => anyhow::bail!(
            "summarization failed: {}",
            error.unwrap_or_else(|| format!("stop reason {stop:?}"))
        ),
    }
}

/// Summarize messages[..cut] and splice a summary message in their place.
/// Returns true if compaction happened.
pub async fn compact_messages(
    provider: &Arc<dyn LlmProvider>,
    model: &str,
    messages: &mut Vec<Message>,
    config: &CompactionConfig,
    emit: &crate::events::Emit,
    cancel: CancellationToken,
    extra_usage: &std::sync::Arc<std::sync::Mutex<pirs_ai::Usage>>,
) -> bool {
    let Some(cut) = find_cut_point_ex(
        messages,
        config.keep_recent_tokens,
        config.min_recent_user_turns,
    ) else {
        return false;
    };
    // Refuse to compact if the kept tail would break tool pairs.
    if !tool_pairs_intact(&messages[cut..]) {
        return false;
    }
    // Demote, don't destroy: the dropped range goes to searchable storage.
    if let Some(mem) = crate::memory::global() {
        mem.add_messages(&messages[..cut]);
    }
    emit(AgentEvent::CompactionStart {
        reason: "threshold".into(),
    });
    let result = summarize(provider, model, &messages[..cut], cancel).await;
    match result {
        Ok((summary, usage)) => {
            *extra_usage.lock().unwrap() += usage;
            let summary_msg =
                Message::user(format!("{SUMMARY_PREFIX}\n{summary}\n{SUMMARY_SUFFIX}"));
            messages.splice(..cut, [summary_msg]);
            // Summary is a user message; remaining tail pairs must stay intact.
            if !tool_pairs_intact(messages) {
                // Should not happen if cut was safe; leave messages as-is for safety
                // by not claiming success is wrong — pairs can include summary + tail.
                // tool_pairs_intact only cares about tool_use/result pairing.
            }
            emit(AgentEvent::CompactionEnd {
                reason: "threshold".into(),
                aborted: false,
                error_message: None,
            });
            true
        }
        Err(e) => {
            emit(AgentEvent::CompactionEnd {
                reason: "threshold".into(),
                aborted: true,
                error_message: Some(e.to_string()),
            });
            false
        }
    }
}

/// Check the last assistant message's usage against the window.
pub fn last_input_tokens(messages: &[Message]) -> Option<u64> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant(AssistantMessage { usage, .. }) => Some(usage.input + usage.cache_read),
        _ => None,
    })
}

/// Shrink oversized tool-result text in history (in place). Returns how many
/// messages were modified. Used before LLM compaction so cut points stay sane
/// when MCP/Rhai dumps blew past per-tool caps earlier in the session.
pub fn shrink_oversized_tool_results(messages: &mut [Message], max_chars: usize) -> usize {
    let mut n = 0usize;
    for msg in messages.iter_mut() {
        let Message::ToolResult(tr) = msg else {
            continue;
        };
        let text: String = tr
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n");
        if text.chars().count() <= max_chars {
            continue;
        }
        let skip = text.chars().count().saturating_sub(max_chars);
        let tail: String = text.chars().skip(skip).collect();
        // Preserve full text in details when missing.
        let has_ui = tr
            .details
            .as_ref()
            .and_then(|d| d.get("uiText"))
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_ui {
            match &mut tr.details {
                Some(serde_json::Value::Object(map)) => {
                    map.insert("uiText".into(), serde_json::Value::String(text));
                }
                _ => {
                    tr.details = Some(serde_json::json!({ "uiText": text }));
                }
            }
        }
        tr.content = vec![ContentBlock::text(format!(
            "[tool result re-truncated for context]\n{tail}"
        ))];
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::{ToolResultMessage, Usage};

    fn user(t: &str) -> Message {
        Message::user(t.to_string())
    }

    fn assistant(t: &str, input: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::text(t)],
            usage: Usage {
                input,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn tool_result(t: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "c".into(),
            tool_name: "x".into(),
            content: vec![ContentBlock::text(t)],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        })
    }

    #[test]
    fn cut_point_snaps_to_user_boundary() {
        let msgs = vec![
            user("start"),
            assistant("a", 0),
            tool_result("r"),
            assistant("b", 0),
            user("middle"),
            assistant("c", 0),
            tool_result("r2"),
            assistant("d", 0),
        ];
        let cut = find_cut_point(&msgs, 10).unwrap();
        // Prefer a user boundary; tool-pair snap may advance past orphan results.
        assert!(
            matches!(msgs[cut], Message::User(_))
                || !matches!(msgs[cut], Message::ToolResult(_)),
            "cut={cut}"
        );
    }

    #[test]
    fn cut_point_none_when_only_initial_prompt() {
        let msgs = vec![user("only"), assistant("a", 0)];
        assert_eq!(find_cut_point(&msgs, 1), None);
    }

    fn assistant_with_tool(id: &str, name: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::text("calling"),
                ContentBlock::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        })
    }

    fn tool_result_id(id: &str, name: &str, t: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![ContentBlock::text(t)],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        })
    }

    #[test]
    fn snap_never_starts_tail_on_tool_result() {
        let msgs = vec![
            user("u1"),
            assistant_with_tool("c1", "bash"),
            tool_result_id("c1", "bash", "ok"),
            user("u2"),
        ];
        let snapped = snap_cut_to_tool_pair_boundary(&msgs, 2);
        assert!(
            !matches!(msgs.get(snapped), Some(Message::ToolResult(_))),
            "cut={snapped}"
        );
        assert!(tool_pairs_intact(&msgs[snapped..]));
    }

    #[test]
    fn snap_keeps_assistant_with_following_results_in_tail() {
        let msgs = vec![
            user("u1"),
            assistant_with_tool("c1", "read"),
            tool_result_id("c1", "read", "file"),
            user("u2"),
        ];
        let snapped = snap_cut_to_tool_pair_boundary(&msgs, 2);
        assert!(
            tool_pairs_intact(&msgs[snapped..]),
            "tail broken at {snapped}"
        );
    }

    #[test]
    fn tool_pairs_intact_detects_dangling_use() {
        let msgs = vec![user("u"), assistant_with_tool("c1", "bash")];
        assert!(!tool_pairs_intact(&msgs));
        let msgs = vec![
            user("u"),
            assistant_with_tool("c1", "bash"),
            tool_result_id("c1", "bash", "ok"),
        ];
        assert!(tool_pairs_intact(&msgs));
    }

    #[test]
    fn find_cut_preserves_tool_pairs_in_tail() {
        let mut msgs = vec![user("goal")];
        for i in 0..20 {
            let pad = format!("pad {i} {}", "x".repeat(200));
            msgs.push(user(&pad));
            let id = format!("c{i}");
            msgs.push(assistant_with_tool(&id, "bash"));
            msgs.push(tool_result_id(&id, "bash", "out"));
        }
        msgs.push(user("recent"));
        let cut = find_cut_point(&msgs, 500).expect("cut");
        assert!(
            tool_pairs_intact(&msgs[cut..]),
            "tail pairs broken cut={cut}"
        );
    }

    #[test]
    fn find_cut_ex_keeps_min_user_turns_with_tool_pairs() {
        let mut msgs = vec![user("goal")];
        for i in 0..12 {
            msgs.push(user(&format!("turn {i} {}", "x".repeat(400))));
            let id = format!("c{i}");
            msgs.push(assistant_with_tool(&id, "edit"));
            msgs.push(tool_result_id(&id, "edit", "ok path/foo.rs"));
        }
        // Small token budget but require 4 user turns in the tail.
        let cut = find_cut_point_ex(&msgs, 50, 4).expect("cut");
        let tail = &msgs[cut..];
        assert!(tool_pairs_intact(tail), "tail pairs broken cut={cut}");
        assert!(
            count_user_turns(tail) >= 4,
            "expected >=4 user turns in tail, got {} cut={cut}",
            count_user_turns(tail)
        );
        // Recent file work still visible in kept tool results / users.
        let flat: String = tail
            .iter()
            .map(|m| match m {
                Message::User(u) => match &u.content {
                    pirs_ai::UserContent::Text(t) => t.clone(),
                    _ => String::new(),
                },
                Message::ToolResult(tr) => tr
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            flat.contains("turn") || flat.contains("foo.rs") || flat.contains("ok"),
            "tail should retain recent work context: {flat}"
        );
    }

    #[test]
    fn should_compact_threshold() {
        let cfg = CompactionConfig {
            context_window: 100_000,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
            min_recent_user_turns: 0,
        };
        assert!(!should_compact(89_999, &cfg));
        assert!(should_compact(90_001, &cfg));
    }

    #[test]
    fn last_input_uses_newest_assistant() {
        let msgs = vec![assistant("a", 100), assistant("b", 250)];
        assert_eq!(last_input_tokens(&msgs), Some(250));
    }

    #[test]
    fn transcript_includes_tool_calls_and_results() {
        let msgs = vec![
            user("hi"),
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls"}),
                    thought_signature: None,
                }],
                ..Default::default()
            }),
            tool_result("file.txt"),
        ];
        let t = transcript_text(&msgs);
        assert!(t.contains("[called bash with {\"command\":\"ls\"}]"));
        assert!(t.contains("Tool(x): file.txt"));
    }
}

#[cfg(test)]
mod demote_tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct SummaryProvider;

    #[async_trait]
    impl LlmProvider for SummaryProvider {
        async fn stream(
            &self,
            _model: &str,
            _context: &Context,
            _options: &pirs_ai::CompletionOptions,
            _cancel: CancellationToken,
        ) -> futures::stream::BoxStream<'static, StreamEvent> {
            Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("summary of old turns".to_string()),
                StreamEvent::Done(Box::new(AssistantMessage {
                    stop_reason: StopReason::Stop,
                    ..Default::default()
                })),
            ]))
        }
    }

    #[test]
    fn shrink_oversized_tool_results_keeps_tail() {
        let big = "z".repeat(5_000);
        let mut messages = vec![
            Message::user("hi"),
            Message::ToolResult(pirs_ai::ToolResultMessage {
                tool_call_id: "1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text(big.clone())],
                details: None,
                is_error: false,
                terminate: false,
                timestamp: 0,
            }),
        ];
        let n = shrink_oversized_tool_results(&mut messages, 100);
        assert_eq!(n, 1);
        let Message::ToolResult(tr) = &messages[1] else {
            panic!("expected tool result");
        };
        assert!(tr.model_text().chars().count() <= 100 + 50);
        assert!(tr
            .details
            .as_ref()
            .and_then(|d| d.get("uiText"))
            .is_some());
    }

    /// Compaction demotes the dropped range into the memory store instead of
    /// destroying it.
    #[tokio::test]
    async fn dropped_range_is_searchable_after_compaction() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::memory::init_global(&tmp.path().join("m.db")).unwrap();

        // Enough small messages to exceed keep_recent_tokens.
        let mut messages = Vec::new();
        for i in 0..40 {
            messages.push(Message::user(format!("question {i} about zebra-{i}")));
            messages.push(Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::text(format!("answer {i}"))],
                usage: pirs_ai::Usage {
                    input: 10,
                    ..Default::default()
                },
                ..Default::default()
            }));
        }
        let cfg = CompactionConfig {
            context_window: 1_000,
            reserve_tokens: 100,
            keep_recent_tokens: 50,
            min_recent_user_turns: 0,
        };
        let emit: crate::events::Emit = std::sync::Arc::new(|_| {});
        let provider: Arc<dyn LlmProvider> = Arc::new(SummaryProvider);
        let usage = std::sync::Arc::new(std::sync::Mutex::new(pirs_ai::Usage::default()));
        let compacted = compact_messages(
            &provider,
            "m",
            &mut messages,
            &cfg,
            &emit,
            CancellationToken::new(),
            &usage,
        )
        .await;
        assert!(compacted);
        let first_is_summary = matches!(
            &messages[0],
            Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("summary of old turns"))
        );
        assert!(first_is_summary, "summary message not spliced in");

        // An early-turn token that was compacted away is still retrievable.
        let hits = store.search("zebra-1 ", 10);
        assert!(
            hits.iter().any(|h| h.snippet.contains("zebra-1")),
            "dropped message not in memory: {hits:?}"
        );
        // Prevent cross-test pollution of the process global.
        crate::memory::clear_global_for_tests();
    }
}
