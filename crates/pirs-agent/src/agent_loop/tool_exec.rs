//! Tool preparation, parallel/serial execution, retries, caps.
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use pirs_ai::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent, ToolResultMessage,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    compact_messages, estimate_tokens, last_input_tokens, should_compact, CompactionConfig,
};
use crate::events::{AgentEvent, Emit, Hooks, ToolResultPatch};
use crate::tool::{tool_defs, AgentTool, ExecutionMode, ToolExecContext};
use crate::validate::{coerce_args, validate_args};

use super::{is_visible, LoopConfig, ToolCallData, VisibleTools, MODEL_MAX_TOOL_RESULT_CHARS};

pub(super) const MODEL_MAX_ERROR_CHARS: usize = 8_000;



pub(super) fn cap_chars_tail(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let skip = n - max_chars;
    s.chars().skip(skip).collect()
}

pub(super) fn merge_result_details(details: &mut Option<serde_json::Value>, extra: serde_json::Value) {
    match details {
        Some(serde_json::Value::Object(existing)) => {
            if let serde_json::Value::Object(add) = extra {
                for (k, v) in add {
                    existing.insert(k, v);
                }
            } else {
                *details = Some(extra);
            }
        }
        _ => *details = Some(extra),
    }
}

/// Truncate model-facing text blocks; spill full text into `details.uiText` when missing.
pub(super) fn apply_model_result_cap(result: &mut ToolResultMessage) {
    let text = result.model_text();
    if text.chars().count() <= MODEL_MAX_TOOL_RESULT_CHARS {
        return;
    }
    let has_ui = result
        .details
        .as_ref()
        .and_then(|d| d.get("uiText"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !has_ui {
        merge_result_details(
            &mut result.details,
            serde_json::json!({ "uiText": text }),
        );
    }
    let capped = cap_chars_tail(&text, MODEL_MAX_TOOL_RESULT_CHARS);
    result.content = vec![ContentBlock::text(format!(
        "[tool result truncated for model context — full output in details.uiText if available]\n{capped}"
    ))];
}

pub(super) fn error_result_kind(id: &str, name: &str, message: &str, kind: &str) -> ToolResultMessage {
    let message = if message.chars().count() > MODEL_MAX_ERROR_CHARS {
        format!(
            "[error truncated]\n{}",
            cap_chars_tail(message, MODEL_MAX_ERROR_CHARS)
        )
    } else {
        message.to_string()
    };
    ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: vec![ContentBlock::text(message)],
        details: Some(serde_json::json!({ "errorKind": kind })),
        is_error: true,
        terminate: false,
        timestamp: pirs_ai::now_millis(),
    }
}

/// The filesystem path a tool call's args target, if any — the key concurrent
/// calls in one batch must not interleave on. All of pirs's file-touching
/// tools (read/edit/write) use a single `path` argument, so this is a plain
/// lookup rather than pirs-tools-specific logic living in the loop.
pub(super) fn tool_path_for_lock(args: &Value) -> Option<String> {
    let raw = args.get("path")?.as_str()?;
    Some(normalize_lock_path(raw))
}

/// Canonicalize a path for use as a lock key, so `src/f.rs`, `./src/f.rs`,
/// and a symlink alias of the same file all collapse to one key instead of
/// silently bypassing the same-path lock. `write` and similar tools target
/// files that may not exist yet, so a missing leaf falls back to
/// canonicalizing the parent directory and re-attaching the leaf name; if
/// even the parent can't be resolved, the raw string is used as-is rather
/// than failing the lookup.
pub(super) fn normalize_lock_path(raw: &str) -> String {
    let path = std::path::Path::new(raw);
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon.to_string_lossy().into_owned();
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
        if let Ok(canon_parent) = std::fs::canonicalize(parent) {
            return canon_parent.join(file_name).to_string_lossy().into_owned();
        }
    }
    raw.to_string()
}

/// Cheap "did you mean" for unknown tool names: longest common prefix / substring.
pub(super) fn did_you_mean<'a>(name: &str, available: &[&'a str]) -> Option<&'a str> {
    let name_l = name.to_ascii_lowercase();
    let mut best: Option<(&str, usize)> = None;
    for &cand in available {
        let c = cand.to_ascii_lowercase();
        let score = if c == name_l {
            1000
        } else if c.contains(&name_l) || name_l.contains(&c) {
            50 + c.len().min(name_l.len())
        } else {
            // shared prefix length
            name_l
                .bytes()
                .zip(c.bytes())
                .take_while(|(a, b)| a == b)
                .count()
        };
        if score >= 3 {
            match best {
                Some((_, s)) if s >= score => {}
                _ => best = Some((cand, score)),
            }
        }
    }
    best.map(|(c, _)| c)
}

pub(super) fn schema_summary(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return "(any object)".to_string();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    props
        .iter()
        .map(|(k, v)| {
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("any");
            let req = if required.contains(&k.as_str()) {
                " (required)"
            } else {
                ""
            };
            format!("{k}: {ty}{req}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) enum Prepared {
    Ready {
        index: usize,
        id: String,
        name: String,
        args: Value,
        tool: Arc<dyn AgentTool>,
    },
    Failed {
        index: usize,
        result: ToolResultMessage,
    },
}

pub(super) fn prepare_call(
    index: usize,
    call: &ToolCallData,
    tools: &[Arc<dyn AgentTool>],
    hooks: &Hooks,
    visible: &Option<VisibleTools>,
) -> Prepared {
    // Last registration wins on a name collision (matches tool_defs's dedup),
    // so a rhai pack can override a native tool — e.g. wrapping `bash` in a
    // sandbox — by registering another tool under the same name later in the
    // list (native tools are constructed first, rhai packs appended after).
    let Some(tool) = tools.iter().rev().find(|t| t.name() == call.name) else {
        let available: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let hint = did_you_mean(&call.name, &available);
        let hint_s = hint
            .map(|h| format!(" Did you mean `{h}`?"))
            .unwrap_or_default();
        return Prepared::Failed {
            index,
            result: error_result_kind(
                &call.id,
                &call.name,
                &format!(
                    "Tool `{}` not found.{hint_s} Available tools: {}.",
                    call.name,
                    available.join(", ")
                ),
                "not_found",
            ),
        };
    };
    if !is_visible(visible, &call.name) {
        return Prepared::Failed {
            index,
            result: error_result_kind(
                &call.id,
                &call.name,
                &format!(
                    "Tool {} is not loaded in this session. Call use_tool(\"{}\") first to load it, then re-issue your call.",
                    call.name, call.name
                ),
                "not_loaded",
            ),
        };
    }
    let schema = tool.parameters();
    // coerce_args runs repair_args first (string/concat/trailing-junk).
    let args = coerce_args(&schema, &call.arguments);
    if let Err(e) = validate_args(&schema, &args) {
        return Prepared::Failed {
            index,
            result: error_result_kind(
                &call.id,
                &call.name,
                &format!(
                    "Invalid arguments for tool {}: {e}. Expected: {}. \
                     Re-issue the call with a single JSON object matching that schema \
                     (no markdown fences, no trailing commentary).",
                    call.name,
                    schema_summary(&schema)
                ),
                "validation",
            ),
        };
    }
    if let Some(before) = &hooks.before_tool_call {
        if let Some(reason) = before(&call.id, &call.name, &args) {
            return Prepared::Failed {
                index,
                result: error_result_kind(
                    &call.id,
                    &call.name,
                    &format!("Tool call blocked: {reason}"),
                    "blocked",
                ),
            };
        }
    }
    Prepared::Ready {
        index,
        id: call.id.clone(),
        name: call.name.clone(),
        args,
        tool: tool.clone(),
    }
}

pub(super) fn finalize_result(
    id: &str,
    name: &str,
    outcome: anyhow::Result<crate::tool::ToolOutput>,
    hooks: &Hooks,
) -> ToolResultMessage {
    let mut result = match outcome {
        Ok(out) => {
            // History for the next LLM turn always uses model-facing content
            // (already capped by tools that call text_with_ui). Longer UI text
            // lives only in details.uiText for TUI/REPL rendering. Loop-level
            // cap below is defense-in-depth for MCP/Rhai/hooks that skip caps.
            ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                content: if out.content.is_empty() {
                    vec![]
                } else {
                    out.content
                },
                details: out.details,
                is_error: false,
                terminate: out.terminate,
                timestamp: pirs_ai::now_millis(),
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let kind = if msg.to_ascii_lowercase().contains("cancel") {
                "cancelled"
            } else if msg.to_ascii_lowercase().contains("timeout")
                || msg.to_ascii_lowercase().contains("timed out")
            {
                "timeout"
            } else {
                "exec"
            };
            error_result_kind(id, name, &msg, kind)
        }
    };
    if let Some(after) = &hooks.after_tool_call {
        if let Some(ToolResultPatch {
            content,
            details,
            is_error,
            terminate,
        }) = after(id, name, &result)
        {
            if let Some(c) = content {
                result.content = c;
            }
            if let Some(d) = details {
                result.details = Some(d);
            }
            if let Some(e) = is_error {
                result.is_error = e;
            }
            if let Some(t) = terminate {
                result.terminate = t;
            }
        }
    }
    // After hooks: re-cap so after_tool_call cannot inject unbounded history.
    if !result.is_error {
        apply_model_result_cap(&mut result);
    } else {
        // Error bodies already capped in error_result_kind; still clamp if hook expanded them.
        let t = result.model_text();
        if t.chars().count() > MODEL_MAX_ERROR_CHARS {
            result.content = vec![ContentBlock::text(format!(
                "[error truncated]\n{}",
                cap_chars_tail(&t, MODEL_MAX_ERROR_CHARS)
            ))];
        }
    }
    result
}

/// Sequential tool batch with optional mid-batch skip (unit-test entry point).
pub async fn execute_tool_calls_for_test(
    calls: Vec<ToolCallData>,
    tools: &[Arc<dyn AgentTool>],
    hooks: &Hooks,
    cancel: CancellationToken,
    emit: &Emit,
    sequential: bool,
    thrash: Option<&crate::thrash::ThrashGuard>,
    skip_remaining_if: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Vec<ToolResultMessage> {
    execute_tool_calls(
        calls,
        tools,
        hooks,
        cancel,
        emit,
        sequential,
        None,
        thrash,
        skip_remaining_if,
    )
    .await
}

pub(super) async fn execute_tool_calls(
    calls: Vec<ToolCallData>,
    tools: &[Arc<dyn AgentTool>],
    hooks: &Hooks,
    cancel: CancellationToken,
    emit: &Emit,
    sequential: bool,
    visible: Option<VisibleTools>,
    thrash: Option<&crate::thrash::ThrashGuard>,
    skip_remaining_if: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> Vec<ToolResultMessage> {
    let n = calls.len();
    let meta: Vec<(String, String)> = calls
        .iter()
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect();
    let mut results: Vec<Option<ToolResultMessage>> = Vec::with_capacity(n);
    results.resize_with(n, || None);

    if sequential {
        let mut skip_rest = false;
        for (index, call) in calls.into_iter().enumerate() {
            if skip_rest {
                let skipped = error_result_kind(
                    &call.id,
                    &call.name,
                    "Skipped due to queued user message.",
                    "skipped_steer",
                );
                emit(AgentEvent::ToolExecutionStart {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    args: call.arguments.clone(),
                });
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: skipped.tool_call_id.clone(),
                    tool_name: skipped.tool_name.clone(),
                    result: Box::new(skipped.clone()),
                });
                results[index] = Some(skipped);
                continue;
            }
            if let Some(g) = thrash {
                if let Some(msg) = g.observe_tool_start(&call.name, &call.arguments) {
                    let failed = error_result_kind(&call.id, &call.name, &msg, "loop_detect");
                    emit(AgentEvent::ToolExecutionStart {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        args: call.arguments.clone(),
                    });
                    emit(AgentEvent::ToolExecutionEnd {
                        tool_call_id: failed.tool_call_id.clone(),
                        tool_name: failed.tool_name.clone(),
                        result: Box::new(failed.clone()),
                    });
                    results[index] = Some(failed);
                    skip_rest = true;
                    continue;
                }
            }
            emit(AgentEvent::ToolExecutionStart {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                args: call.arguments.clone(),
            });
            let result = match prepare_call(index, &call, tools, hooks, &visible) {
                Prepared::Failed { result, .. } => result,
                Prepared::Ready {
                    id,
                    name,
                    args,
                    tool,
                    ..
                } => {
                    let outcome =
                        run_tool(tool, id.clone(), name.clone(), args, cancel.clone(), emit).await;
                    finalize_result(&id, &name, outcome, hooks)
                }
            };
            if let Some(g) = thrash {
                let _ = g.observe_tool_end(result.is_error);
            }
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
                result: Box::new(result.clone()),
            });
            results[index] = Some(result);
            if cancel.is_cancelled() {
                break;
            }
            // After each tool: if steering is pending, skip the rest of the batch.
            if let Some(pred) = skip_remaining_if {
                if pred() {
                    skip_rest = true;
                }
            }
        }
    } else {
        // Parallel path still observes thrash (criterion 2: default execution mode)
        // and honors mid-batch steer skip before launching work.
        let mut prepared = Vec::new();
        let mut thrash_blocked = false;
        let mut steer_skip = false;
        for (index, call) in calls.into_iter().enumerate() {
            if !steer_skip {
                if let Some(pred) = skip_remaining_if {
                    if pred() {
                        steer_skip = true;
                    }
                }
            }
            emit(AgentEvent::ToolExecutionStart {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                args: call.arguments.clone(),
            });
            if thrash_blocked {
                let skipped = error_result_kind(
                    &call.id,
                    &call.name,
                    "Skipped due to thrash stop.",
                    "skipped_thrash",
                );
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: skipped.tool_call_id.clone(),
                    tool_name: skipped.tool_name.clone(),
                    result: Box::new(skipped.clone()),
                });
                results[index] = Some(skipped);
                continue;
            }
            if steer_skip {
                let skipped = error_result_kind(
                    &call.id,
                    &call.name,
                    "Skipped due to queued user message.",
                    "skipped_steer",
                );
                emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: skipped.tool_call_id.clone(),
                    tool_name: skipped.tool_name.clone(),
                    result: Box::new(skipped.clone()),
                });
                results[index] = Some(skipped);
                continue;
            }
            if let Some(g) = thrash {
                if let Some(msg) = g.observe_tool_start(&call.name, &call.arguments) {
                    let failed = error_result_kind(&call.id, &call.name, &msg, "loop_detect");
                    emit(AgentEvent::ToolExecutionEnd {
                        tool_call_id: failed.tool_call_id.clone(),
                        tool_name: failed.tool_name.clone(),
                        result: Box::new(failed.clone()),
                    });
                    results[index] = Some(failed);
                    thrash_blocked = true;
                    continue;
                }
            }
            match prepare_call(index, &call, tools, hooks, &visible) {
                Prepared::Failed { index, result } => {
                    if let Some(g) = thrash {
                        let _ = g.observe_tool_end(result.is_error);
                    }
                    emit(AgentEvent::ToolExecutionEnd {
                        tool_call_id: result.tool_call_id.clone(),
                        tool_name: result.tool_name.clone(),
                        result: Box::new(result.clone()),
                    });
                    results[index] = Some(result);
                }
                ready => prepared.push(ready),
            }
        }
        // Same-path calls in this batch must not interleave (two concurrent
        // edits to one file can otherwise race and clobber each other);
        // different paths run fully concurrently. Built once, up front, so
        // every task in the batch shares the same lock instance per path.
        let mut path_locks: std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>> =
            std::collections::HashMap::new();
        for p in &prepared {
            if let Prepared::Ready { args, .. } = p {
                if let Some(path) = tool_path_for_lock(args) {
                    path_locks
                        .entry(path)
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())));
                }
            }
        }

        let mut in_flight = FuturesUnordered::new();
        for p in prepared {
            // Re-check steer before launching each prepared tool.
            if let Some(pred) = skip_remaining_if {
                if pred() {
                    if let Prepared::Ready {
                        index, id, name, ..
                    } = p
                    {
                        let skipped = error_result_kind(
                            &id,
                            &name,
                            "Skipped due to queued user message.",
                            "skipped_steer",
                        );
                        emit(AgentEvent::ToolExecutionEnd {
                            tool_call_id: skipped.tool_call_id.clone(),
                            tool_name: skipped.tool_name.clone(),
                            result: Box::new(skipped.clone()),
                        });
                        results[index] = Some(skipped);
                    }
                    continue;
                }
            }
            if let Prepared::Ready {
                index,
                id,
                name,
                args,
                tool,
            } = p
            {
                let cancel = cancel.clone();
                let path_lock = tool_path_for_lock(&args).map(|path| path_locks[&path].clone());
                in_flight.push(async move {
                    let _guard = match &path_lock {
                        Some(lock) => Some(lock.lock().await),
                        None => None,
                    };
                    let outcome =
                        run_tool(tool, id.clone(), name.clone(), args, cancel, emit).await;
                    (index, id, name, outcome)
                });
            }
        }
        while let Some((index, id, name, outcome)) = in_flight.next().await {
            let result = finalize_result(&id, &name, outcome, hooks);
            if let Some(g) = thrash {
                let _ = g.observe_tool_end(result.is_error);
            }
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: id,
                tool_name: name,
                result: Box::new(result.clone()),
            });
            results[index] = Some(result);
        }
    }

    let cancelled = cancel.is_cancelled();
    results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            r.unwrap_or_else(|| {
                let (id, name) = &meta[i];
                if cancelled {
                    error_result_kind(id, name, "Tool execution cancelled", "cancelled")
                } else {
                    error_result_kind(id, name, "Tool execution did not complete", "incomplete")
                }
            })
        })
        .collect()
}

pub(super) async fn run_tool(
    tool: Arc<dyn AgentTool>,
    id: String,
    name: String,
    args: Value,
    cancel: CancellationToken,
    emit: &Emit,
) -> anyhow::Result<crate::tool::ToolOutput> {
    let on_update: Arc<dyn Fn(String) + Send + Sync> = {
        let id = id.clone();
        let name = name.clone();
        let emit = emit.clone();
        Arc::new(move |partial: String| {
            emit(AgentEvent::ToolExecutionUpdate {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                partial,
            });
        })
    };
    let ctx = ToolExecContext {
        tool_call_id: id.clone(),
        args: args.clone(),
        cancel: cancel.clone(),
        on_update: Some(on_update.clone()),
    };
    match tool.execute(ctx).await {
        Ok(out) => Ok(out),
        Err(e)
            if is_idempotent_tool(&name)
                && is_transient_tool_error(&e)
                && !cancel.is_cancelled() =>
        {
            // One automatic retry only for known-idempotent tools (M-5).
            // Never re-run edit/write/bash on substring matches like "HTTP 503"
            // printed by a successful script.
            let ctx2 = ToolExecContext {
                tool_call_id: id,
                args,
                cancel,
                on_update: Some(on_update),
            };
            tool.execute(ctx2).await
        }
        Err(e) => Err(e),
    }
}

/// Tools safe to re-execute once on network/timeout errors (no side effects).
pub(super) fn is_idempotent_tool(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "grep"
            | "find"
            | "ls"
            | "web_fetch"
            | "web_search"
            | "doctor"
            | "recall"
            | "code_map"
            | "code_search"
            | "session_search"
            | "mcp_search"
            | "mcp_describe"
            | "mcp_status"
            | "vision_describe"
            | "project"
    ) || name.starts_with("lsp")
}

/// Network/timeout-class failures worth one automatic retry (idempotent tools only).
pub(super) fn is_transient_tool_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    // Prefer explicit timeout/transport wording over bare "503" (bash scripts
    // often print HTTP status codes in success output that becomes Err text).
    s.contains("timeout")
        || s.contains("timed out")
        || s.contains("connection reset")
        || s.contains("connection refused")
        || s.contains("broken pipe")
        || s.contains("temporarily unavailable")
        || s.contains("econnreset")
        || s.contains("dns error")
        || s.contains("network unreachable")
        || s.contains("error sending request")
        || s.contains("error decoding response")
}


/// Validate a single tool call against the registered tools without executing it.
///
/// Returns `Ok(())` when the call would be dispatched, or `Err(message)` when it
/// is rejected (unknown tool, not loaded, invalid args). Invalid payloads are
/// never treated as successful tool use — callers attach the error as a
/// `tool_result` so the model can retry.
pub fn validate_tool_call_payload(
    name: &str,
    arguments: &Value,
    tools: &[Arc<dyn AgentTool>],
    visible: &Option<VisibleTools>,
) -> Result<(), String> {
    let call = ToolCallData {
        id: "validate".into(),
        name: name.to_string(),
        arguments: arguments.clone(),
    };
    match prepare_call(0, &call, tools, &Hooks::default(), visible) {
        Prepared::Ready { .. } => Ok(()),
        Prepared::Failed { result, .. } => {
            let msg: String = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            Err(if msg.is_empty() {
                "tool call rejected".into()
            } else {
                msg
            })
        }
    }
}
