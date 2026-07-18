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
    pub keep_recent_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
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
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> Option<usize> {
    let mut acc = 0u64;
    let mut cut = None;
    for i in (0..messages.len()).rev() {
        acc += estimate_message_tokens(&messages[i]);
        if matches!(messages[i], Message::User(_)) {
            cut = Some(i);
            if acc >= keep_recent_tokens {
                break;
            }
        }
    }
    match cut {
        Some(0) | None => None,
        other => other,
    }
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
    let Some(cut) = find_cut_point(messages, config.keep_recent_tokens) else {
        return false;
    };
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
        assert!(matches!(msgs[cut], Message::User(_)));
        assert_eq!(cut, 4);
    }

    #[test]
    fn cut_point_none_when_only_initial_prompt() {
        let msgs = vec![user("only"), assistant("a", 0)];
        assert_eq!(find_cut_point(&msgs, 1), None);
    }

    #[test]
    fn should_compact_threshold() {
        let cfg = CompactionConfig {
            context_window: 100_000,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
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
