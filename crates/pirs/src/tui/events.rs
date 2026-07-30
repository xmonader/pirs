use std::sync::{Arc, Mutex};

use pirs_agent::AgentEvent;
use pirs_ai::Message;

use super::app::{App, TuiOptions};
use super::chat::ChatItem;
use super::tools::*;

// ── Agent events ────────────────────────────────────────────────────────────

pub(super) fn apply_agent_event(app: &mut App, event: AgentEvent) {
    match event {
        AgentEvent::MessageStart { message } => {
            if message.is_assistant() {
                app.live = Some((String::new(), String::new()));
                app.last_activity = "streaming".into();
                app.dirty = true;
            }
        }
        AgentEvent::MessageUpdate { message } => {
            if app.live.is_none() {
                return;
            }
            let thinking = extract_thinking(&message);
            let text = message.text();
            if !thinking.is_empty() && text.trim().is_empty() {
                app.last_activity = "thinking".into();
            } else {
                app.last_activity = "streaming".into();
            }
            if app.last_live_refresh.elapsed() < std::time::Duration::from_millis(80) {
                // Always keep latest content even if we skip a paint.
                app.live = Some((thinking, text));
                return;
            }
            app.last_live_refresh = std::time::Instant::now();
            app.live = Some((thinking, text));
            app.dirty = true;
        }
        AgentEvent::MessageEnd { message } => {
            if let Message::Assistant(a) = *message {
                app.live = None;
                let thinking = extract_thinking(&a);
                let text = a.text();
                let error = if a.stop_reason == pirs_ai::StopReason::Error {
                    Some(a.error_message.unwrap_or_else(|| "unknown error".into()))
                } else {
                    None
                };
                if !thinking.trim().is_empty() || !text.trim().is_empty() || error.is_some() {
                    app.push(ChatItem::Assistant {
                        thinking,
                        text,
                        error,
                    });
                }
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            let summary = crate::summarize_args(&tool_name, &args);
            app.start_tool(tool_name, summary);
        }
        AgentEvent::ToolExecutionEnd {
            result, tool_name, ..
        } => {
            app.clock.mark_tool(result.is_error);
            // Prefer details.uiText (full) over model-capped content for display.
            let text = result.display_text();
            let preview: String = text
                .lines()
                .take(TOOL_PREVIEW_CAP)
                .collect::<Vec<_>>()
                .join("\n");
            let body = if preview.is_empty() && result.is_error {
                "(error)".into()
            } else {
                preview
            };
            if !body.is_empty() || result.is_error {
                app.finish_tool(&tool_name, body, result.is_error);
            } else {
                // Success with empty body — still mark done.
                app.finish_tool(&tool_name, String::new(), false);
            }
        }
        AgentEvent::CompactionStart { .. } => {
            app.last_activity = "compacting".into();
            app.notice("compacting context…");
        }
        AgentEvent::CompactionEnd { aborted, .. } => {
            if aborted {
                app.notice("compaction skipped");
            } else {
                app.notice("compaction done");
            }
        }
        AgentEvent::TurnStart => {
            app.last_activity = "thinking".into();
            app.set_status("thinking");
        }
        AgentEvent::TurnEnd { .. } => {
            app.last_activity = "running".into();
            app.set_status("running");
        }
        AgentEvent::AgentStart => {
            if app.turn_started_at.is_none() {
                app.turn_started_at = Some(std::time::Instant::now());
            }
            app.last_activity = "running".into();
            app.set_status("running");
        }
        AgentEvent::AgentEnd { .. } => {
            app.set_status(String::new());
        }
        _ => {}
    }
}

pub(super) fn extract_thinking(a: &pirs_ai::AssistantMessage) -> String {
    let mut parts: Vec<String> = a
        .content
        .iter()
        .filter_map(|b| match b {
            pirs_ai::ContentBlock::Thinking {
                thinking, redacted, ..
            } if !thinking.trim().is_empty() => {
                if *redacted {
                    Some(String::from("[redacted thinking]"))
                } else {
                    Some(thinking.clone())
                }
            }
            _ => None,
        })
        .collect();
    // Fallback: some models stuff reasoning into text as <think>…</think>.
    if parts.is_empty() {
        let text = a.text();
        if let Some(inner) = extract_think_tags(&text) {
            parts.push(inner);
        }
    }
    parts.join("\n")
}

/// Pull `<think>…</think>` / `<thinking>…</thinking>` bodies from assistant text.
pub(super) fn extract_think_tags(text: &str) -> Option<String> {
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        if let Some(start) = text.find(open) {
            let after = start + open.len();
            if let Some(end) = text[after..].find(close) {
                let body = text[after..after + end].trim();
                if !body.is_empty() {
                    return Some(body.to_string());
                }
            } else {
                // Unclosed tag while streaming — show partial.
                let body = text[after..].trim();
                if !body.is_empty() {
                    return Some(body.to_string());
                }
            }
        }
    }
    None
}

// ── Approval bridge ─────────────────────────────────────────────────────────

pub(super) fn approval_bridge(
    opts: &mut TuiOptions,
) -> (
    Arc<Mutex<Option<String>>>,
    Arc<std::sync::mpsc::Sender<String>>,
) {
    let pending: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(gate) = &opts.approval_gate {
        let pending = Arc::clone(&pending);
        let rx = Arc::new(std::sync::Mutex::new(rx));
        gate.set_prompter(move |question| {
            *pending.lock().unwrap() = Some(question.to_string());
            rx.lock()
                .unwrap()
                .recv()
                .unwrap_or_else(|_| "n".to_string())
        });
    }
    (pending, Arc::new(tx))
}
