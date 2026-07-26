//! Freeform tool text repair and payload validation.
use pirs_ai::{AssistantMessage, ContentBlock};

use super::ToolCallData;

pub(super) fn extract_tool_calls(assistant: &AssistantMessage) -> Vec<ToolCallData> {
    assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(ToolCallData {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Detect freeform / non-native "tool" text that was not emitted as ToolCall blocks.
///
/// Used to re-prompt weak models that paste markdown JSON or shell-style
/// invocations instead of using the provider tool protocol.
pub fn looks_like_freeform_tool_text(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains("```")
        && (t.contains("\"function\"")
            || t.contains("\"name\"")
            || t.contains("\"tool\"")
            || t.contains("\"arguments\""))
    {
        return true;
    }
    if t.starts_with('[')
        && t.contains('{')
        && (t.contains("\"function\"") || t.contains("\"name\""))
        && (t.contains("\"path\"")
            || t.contains("\"command\"")
            || t.contains("\"arguments\"")
            || t.contains("\"tool\""))
    {
        return true;
    }
    if t.lines().any(|l| {
        let s = l.trim_start();
        s.starts_with("> bash")
            || s.starts_with("> read")
            || s.starts_with("> edit")
            || s.starts_with("> grep")
    }) {
        return true;
    }
    false
}

pub(super) fn freeform_tool_repair_nudge(assistant: &AssistantMessage) -> Option<String> {
    let text: String = assistant
        .content
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    if !looks_like_freeform_tool_text(&text) {
        return None;
    }
    Some(
        "[system tool protocol] Your previous reply looks like a tool invocation written as \
         freeform text (markdown/JSON/shell), not as a native tool call. That is not executed. \
         Re-issue using the provider's native tool/function calling protocol only — \
         no markdown fences, no pseudo `> bash` lines, no JSON arrays of tools as plain text."
            .into(),
    )
}
