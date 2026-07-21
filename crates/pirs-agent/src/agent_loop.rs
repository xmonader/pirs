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

pub struct LoopConfig {
    pub model: String,
    pub completion: CompletionOptions,
    pub tool_execution: ExecutionMode,
    pub hooks: Hooks,
    pub compaction: Option<CompactionConfig>,
    pub visible_tools: Option<VisibleTools>,
    pub extra_usage: std::sync::Arc<std::sync::Mutex<pirs_ai::Usage>>,
    pub cascade: Option<CascadeConfig>,
    pub budgets: Budgets,
    /// Loop/mistake thrash guard (default-on when set by Agent).
    pub thrash: Option<crate::thrash::ThrashGuard>,
    /// When sequential tools run, if this returns true after a tool finishes,
    /// remaining tools in the batch are skipped (steering pending, etc.).
    pub skip_remaining_if: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>,
}

#[derive(Debug, Clone, Default)]
pub struct Budgets {
    pub max_turns: Option<usize>,
    pub max_tool_calls: Option<usize>,
    pub max_wall_time: Option<std::time::Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetHit {
    Turns,
    WallTime,
    ToolCalls,
}

pub type CascadeJudge =
    Arc<dyn Fn(&AssistantMessage) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

#[derive(Clone)]
pub struct CascadeConfig {
    pub draft_model: String,
    pub judge: CascadeJudge,
}

pub type VisibleTools = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

pub fn is_visible(visible: &Option<VisibleTools>, name: &str) -> bool {
    match visible {
        None => true,
        Some(set) => set.lock().unwrap().contains(name),
    }
}

pub struct ToolCallData {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

pub async fn run_agent_loop(
    prompts: Vec<Message>,
    context: &mut Context,
    tools: &[Arc<dyn AgentTool>],
    provider: &Arc<dyn LlmProvider>,
    config: &LoopConfig,
    emit: &Emit,
    cancel: CancellationToken,
) -> (Vec<Message>, Option<BudgetHit>) {
    let mut new_messages: Vec<Message> = Vec::new();

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for prompt in prompts {
        emit(AgentEvent::MessageStart {
            message: Box::new(prompt.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: Box::new(prompt.clone()),
        });
        context.messages.push(prompt.clone());
        new_messages.push(prompt);
    }

    let mut pending = config.hooks.steering();
    let mut first_turn = true;
    let mut turn_count = 0usize;
    let mut tool_call_count = 0usize;
    let started = std::time::Instant::now();
    let mut budget_hit = None;

    'outer: loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() || first_turn {
            first_turn = false;
            for msg in pending.drain(..) {
                emit(AgentEvent::MessageStart {
                    message: Box::new(msg.clone()),
                });
                context.messages.push(msg.clone());
                emit(AgentEvent::MessageEnd {
                    message: Box::new(msg.clone()),
                });
                new_messages.push(msg);
            }

            let assistant =
                stream_assistant(context, tools, provider, config, emit, cancel.clone()).await;
            emit(AgentEvent::MessageEnd {
                message: Box::new(Message::Assistant(assistant.clone())),
            });
            new_messages.push(Message::Assistant(assistant.clone()));

            if matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                // An errored/aborted assistant can still carry ToolCall blocks
                // (partial Done). Persisting a tool_use with no following
                // tool_result makes the next Anthropic request 400 forever,
                // permanently wedging the session. Synthesize error results for
                // any dangling calls so the history stays valid.
                let dangling = extract_tool_calls(&assistant);
                let mut tool_results = Vec::new();
                for call in &dangling {
                    let r = error_result_kind(
                        &call.id,
                        &call.name,
                        "Tool call was not executed: the turn ended with an error or was aborted.",
                        "aborted",
                    );
                    let msg = Message::ToolResult(r.clone());
                    context.messages.push(msg.clone());
                    new_messages.push(msg);
                    tool_results.push(r);
                }
                emit(AgentEvent::TurnEnd {
                    message: Box::new(assistant),
                    tool_results,
                });
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return (new_messages, None);
            }

            let calls = extract_tool_calls(&assistant);
            let had_calls = !calls.is_empty();
            let mut results: Vec<ToolResultMessage> = Vec::new();
            if had_calls {
                if assistant.stop_reason == StopReason::Length {
                    for call in &calls {
                        results.push(error_result_kind(
                            &call.id,
                            &call.name,
                            "Tool call arguments were truncated due to token limit. Re-issue the tool call.",
                            "truncated",
                        ));
                    }
                } else {
                    let forced_sequential = config.tool_execution == ExecutionMode::Sequential
                        || calls.iter().any(|c| {
                            tools
                                .iter()
                                .find(|t| t.name() == c.name)
                                .map(|t| t.execution_mode() == ExecutionMode::Sequential)
                                .unwrap_or(false)
                        });
                    results = execute_tool_calls(
                        calls,
                        tools,
                        &config.hooks,
                        cancel.clone(),
                        emit,
                        forced_sequential,
                        config.visible_tools.clone(),
                        config.thrash.as_ref(),
                        config.skip_remaining_if.as_ref().map(|f| f.as_ref()),
                    )
                    .await;
                }
                // Always attach tool_results before any thrash stop. Returning early
                // here used to leave assistant tool_use without matching results and
                // permanently wedge the next Anthropic request (400 forever).
                for r in &results {
                    // Spill every tool result to searchable session memory —
                    // except recall's own output, which would recursively
                    // pollute the store with copies of past hits.
                    if r.tool_name != "recall" {
                        if let Some(mem) = crate::memory::global() {
                            let text: String = r
                                .content
                                .iter()
                                .filter_map(|b| b.as_text())
                                .collect::<Vec<_>>()
                                .join("\n");
                            mem.add("tool_result", &r.tool_name, &text);
                        }
                    }
                    let msg = Message::ToolResult(r.clone());
                    emit(AgentEvent::MessageStart {
                        message: Box::new(msg.clone()),
                    });
                    context.messages.push(msg.clone());
                    emit(AgentEvent::MessageEnd {
                        message: Box::new(msg.clone()),
                    });
                    new_messages.push(msg);
                }
            }

            emit(AgentEvent::TurnEnd {
                message: Box::new(assistant.clone()),
                tool_results: results.clone(),
            });
            // Thrash stop after tool_results are on the wire (protocol-safe).
            if had_calls {
                if let Some(guard) = &config.thrash {
                    if let Some(msg) = guard.take_stop() {
                        let stop = Message::user(format!("[system thrash stop] {msg}"));
                        emit(AgentEvent::MessageStart {
                            message: Box::new(stop.clone()),
                        });
                        context.messages.push(stop.clone());
                        emit(AgentEvent::MessageEnd {
                            message: Box::new(stop.clone()),
                        });
                        new_messages.push(stop);
                        emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        });
                        return (new_messages, None);
                    }
                }
            }
            turn_count += 1;
            tool_call_count += results.len();
            if config
                .budgets
                .max_turns
                .map(|m| turn_count >= m)
                .unwrap_or(false)
            {
                budget_hit = Some(BudgetHit::Turns);
            } else if config
                .budgets
                .max_tool_calls
                .map(|m| tool_call_count >= m)
                .unwrap_or(false)
            {
                budget_hit = Some(BudgetHit::ToolCalls);
            } else if config
                .budgets
                .max_wall_time
                .map(|m| started.elapsed() >= m)
                .unwrap_or(false)
            {
                budget_hit = Some(BudgetHit::WallTime);
            }
            if budget_hit.is_some() {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return (new_messages, budget_hit);
            }

            if let Some(cfg) = &config.compaction {
                // Prefer provider-reported input tokens; fall back to local estimate
                // so huge tool dumps still trigger compaction when usage is missing.
                let over = last_input_tokens(&context.messages)
                    .map(|t| should_compact(t, cfg))
                    .unwrap_or(false)
                    || should_compact(estimate_tokens(&context.messages), cfg);
                if over {
                    // Defense: shrink oversized tool results in history first
                    // (cheap) so cut-point retention stays meaningful.
                    let _shrunk = crate::compaction::shrink_oversized_tool_results(
                        &mut context.messages,
                        MODEL_MAX_TOOL_RESULT_CHARS,
                    );
                    compact_messages(
                        provider,
                        &config.model,
                        &mut context.messages,
                        cfg,
                        emit,
                        cancel.clone(),
                        &config.extra_usage,
                    )
                    .await;
                }
            }

            let batch_terminate = !results.is_empty() && results.iter().all(|r| r.terminate);
            has_more_tool_calls = had_calls && !batch_terminate;

            if let Some(f) = &config.hooks.should_stop_after_turn {
                if f(context) {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    });
                    return (new_messages, None);
                }
            }
            pending = config.hooks.steering();
        }

        let follow = config.hooks.follow_up();
        if follow.is_empty() {
            emit(AgentEvent::AgentEnd {
                messages: new_messages.clone(),
            });
            break 'outer;
        }
        pending = follow;
    }

    (new_messages, budget_hit)
}

fn extract_tool_calls(assistant: &AssistantMessage) -> Vec<ToolCallData> {
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

async fn stream_assistant(
    context: &mut Context,
    tools: &[Arc<dyn AgentTool>],
    provider: &Arc<dyn LlmProvider>,
    config: &LoopConfig,
    emit: &Emit,
    cancel: CancellationToken,
) -> AssistantMessage {
    if let Some(cascade) = &config.cascade {
        let draft = stream_once(
            context,
            tools,
            provider,
            config,
            &cascade.draft_model,
            emit,
            cancel.clone(),
        )
        .await;
        if (cascade.judge)(&draft).await {
            return draft;
        }
        if let Some(last) = context.messages.last() {
            if last.is_assistant() {
                context.messages.pop();
            }
        }
        emit(AgentEvent::MessageEnd {
            message: Box::new(Message::Assistant(draft)),
        });
    }
    stream_once(
        context,
        tools,
        provider,
        config,
        &config.model.clone(),
        emit,
        cancel,
    )
    .await
}

async fn stream_once(
    context: &mut Context,
    tools: &[Arc<dyn AgentTool>],
    provider: &Arc<dyn LlmProvider>,
    config: &LoopConfig,
    model: &str,
    emit: &Emit,
    cancel: CancellationToken,
) -> AssistantMessage {
    let mut opts = config.completion.clone();
    if let Some(f) = &config.hooks.get_api_key {
        if let Some(key) = f() {
            opts.api_key = Some(key);
        }
    }

    // Packs may rewrite the LLM-facing list (plan pins, janitor, …). Snapshot
    // first so the host can restore protected control pins (stop_gate, verify,
    // thrash nudges) if a transform strips them.
    let original_messages = context.messages.clone();
    let mut messages = original_messages.clone();
    if let Some(t) = &config.hooks.transform_context {
        messages = t(messages);
    }
    messages = crate::control_pins::preserve_control_pins(&original_messages, messages);
    let llm_ctx = Context {
        system_prompt: context.system_prompt.clone(),
        messages,
        tools: tool_defs(tools)
            .into_iter()
            .filter(|d| is_visible(&config.visible_tools, &d.name))
            .collect(),
    };

    let mut stream = provider
        .stream(model, &llm_ctx, &opts, cancel.clone())
        .await;

    let mut partial = AssistantMessage {
        provider: "unknown".into(),
        model: model.to_string(),
        ..Default::default()
    };
    context.messages.push(Message::Assistant(partial.clone()));
    emit(AgentEvent::MessageStart {
        message: Box::new(Message::Assistant(partial.clone())),
    });

    let mut last_error: Option<String> = None;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Start | StreamEvent::ToolCallDelta => {}
            StreamEvent::TextDelta(d) => {
                append_text(&mut partial, d.clone());
                append_delta_to_last(context, &d, false);
                emit(AgentEvent::MessageUpdate {
                    message: Box::new(partial.clone()),
                });
            }
            StreamEvent::ThinkingDelta(d) => {
                append_thinking(&mut partial, d.clone());
                append_delta_to_last(context, &d, true);
                emit(AgentEvent::MessageUpdate {
                    message: Box::new(partial.clone()),
                });
            }
            StreamEvent::Error(e) => {
                last_error = Some(e);
            }
            StreamEvent::Done(msg) => {
                partial = *msg;
            }
        }
    }
    if let Some(err) = last_error {
        if partial.error_message.is_none() {
            partial.error_message = Some(err);
        }
        // A transport drop after an Error frame (no Done) leaves stop_reason at
        // its default Stop, so the loop would treat a failed turn as a clean
        // final answer. Force Error unless a Done already set a terminal reason.
        if partial.stop_reason == StopReason::Stop {
            partial.stop_reason = StopReason::Error;
        }
    }

    replace_last(context, &partial);
    partial
}

fn append_text(msg: &mut AssistantMessage, delta: String) {
    match msg.content.last_mut() {
        Some(ContentBlock::Text { text, .. }) => text.push_str(&delta),
        _ => msg.content.push(ContentBlock::text(delta)),
    }
}

fn append_thinking(msg: &mut AssistantMessage, delta: String) {
    match msg.content.last_mut() {
        Some(ContentBlock::Thinking { thinking, .. }) => thinking.push_str(&delta),
        _ => msg.content.push(ContentBlock::Thinking {
            thinking: delta,
            thinking_signature: None,
            redacted: false,
        }),
    }
}

fn replace_last(context: &mut Context, msg: &AssistantMessage) {
    if let Some(last) = context.messages.last_mut() {
        *last = Message::Assistant(msg.clone());
    }
}

/// O(1) delta application to the trailing assistant message in context —
/// avoids cloning the whole AssistantMessage on every streamed token.
fn append_delta_to_last(context: &mut Context, delta: &str, thinking: bool) {
    if let Some(Message::Assistant(a)) = context.messages.last_mut() {
        if thinking {
            append_thinking(a, delta.to_string());
        } else {
            append_text(a, delta.to_string());
        }
    }
}

/// Defense-in-depth cap for model-facing tool results (MCP/Rhai/hooks included).
/// Per-tool caps (e.g. bash `cap_for_model`) still apply first; this is the backstop.
pub const MODEL_MAX_TOOL_RESULT_CHARS: usize = 20_000;
/// Cap for error result bodies so a failed bash dump cannot blow the next turn.
pub const MODEL_MAX_ERROR_CHARS: usize = 8_000;

fn cap_chars_tail(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let skip = n - max_chars;
    s.chars().skip(skip).collect()
}

fn merge_result_details(details: &mut Option<serde_json::Value>, extra: serde_json::Value) {
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
fn apply_model_result_cap(result: &mut ToolResultMessage) {
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

fn error_result_kind(id: &str, name: &str, message: &str, kind: &str) -> ToolResultMessage {
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
fn tool_path_for_lock(args: &Value) -> Option<String> {
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
fn normalize_lock_path(raw: &str) -> String {
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
fn did_you_mean<'a>(name: &str, available: &[&'a str]) -> Option<&'a str> {
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

fn schema_summary(schema: &Value) -> String {
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

enum Prepared {
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

fn prepare_call(
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

fn finalize_result(
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

async fn execute_tool_calls(
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

async fn run_tool(
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
        Err(e) if is_transient_tool_error(&e) && !cancel.is_cancelled() => {
            // One automatic retry for timeout/network-class failures.
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

/// Network/timeout-class failures worth one automatic retry.
fn is_transient_tool_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("timeout")
        || s.contains("timed out")
        || s.contains("connection reset")
        || s.contains("connection refused")
        || s.contains("broken pipe")
        || s.contains("temporarily unavailable")
        || s.contains("try again")
        || s.contains("503")
        || s.contains("502")
        || s.contains("504")
        || s.contains("econnreset")
        || s.contains("dns error")
}

#[cfg(test)]
mod result_cap_tests {
    use super::*;
    use pirs_ai::ContentBlock;

    #[test]
    fn cap_chars_tail_keeps_end() {
        let s: String = (0..100).map(|i| format!("{i}")).collect();
        let t = cap_chars_tail(&s, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(s.ends_with(&t) || t.chars().all(|c| s.contains(c)));
    }

    #[test]
    fn apply_model_result_cap_spills_ui_text() {
        let big = "x".repeat(MODEL_MAX_TOOL_RESULT_CHARS + 500);
        let mut result = ToolResultMessage {
            tool_call_id: "1".into(),
            tool_name: "t".into(),
            content: vec![ContentBlock::text(big.clone())],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        };
        apply_model_result_cap(&mut result);
        assert!(result.model_text().chars().count() <= MODEL_MAX_TOOL_RESULT_CHARS + 120);
        assert!(result.model_text().contains("truncated"));
        let ui = result
            .details
            .as_ref()
            .and_then(|d| d.get("uiText"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(ui.len(), big.len());
    }

    #[test]
    fn error_result_kind_tags_and_truncates() {
        let big = "e".repeat(MODEL_MAX_ERROR_CHARS + 200);
        let r = error_result_kind("id", "bash", &big, "exec");
        assert!(r.is_error);
        assert!(r.model_text().chars().count() <= MODEL_MAX_ERROR_CHARS + 40);
        assert_eq!(
            r.details
                .as_ref()
                .and_then(|d| d.get("errorKind"))
                .and_then(|v| v.as_str()),
            Some("exec")
        );
    }

    #[test]
    fn finalize_result_caps_ok_output() {
        let big = "z".repeat(MODEL_MAX_TOOL_RESULT_CHARS + 1000);
        let out = crate::tool::ToolOutput::text(big);
        let r = finalize_result("c1", "mcp_tool", Ok(out), &Hooks::default());
        assert!(!r.is_error);
        assert!(r.model_text().chars().count() <= MODEL_MAX_TOOL_RESULT_CHARS + 120);
        assert!(r
            .details
            .as_ref()
            .and_then(|d| d.get("uiText"))
            .is_some());
    }

    #[test]
    fn transient_tool_error_classifier() {
        assert!(is_transient_tool_error(&anyhow::anyhow!("connection reset by peer")));
        assert!(is_transient_tool_error(&anyhow::anyhow!("HTTP 503")));
        assert!(is_transient_tool_error(&anyhow::anyhow!("request timed out")));
        assert!(!is_transient_tool_error(&anyhow::anyhow!("file not found")));
        assert!(!is_transient_tool_error(&anyhow::anyhow!("Invalid arguments")));
    }
}


#[cfg(test)]
mod skip_remaining_tests {
    use super::*;
    use async_trait::async_trait;
    use crate::tool::{AgentTool, ToolExecContext, ToolOutput};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountTool {
        name: String,
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentTool for CountTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "count"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("ok"))
        }
    }

    #[tokio::test]
    async fn sequential_skips_remaining_when_predicate_true() {
        let hits = Arc::new(AtomicUsize::new(0));
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(CountTool { name: "a".into(), hits: Arc::clone(&hits) }),
            Arc::new(CountTool { name: "b".into(), hits: Arc::clone(&hits) }),
            Arc::new(CountTool { name: "c".into(), hits: Arc::clone(&hits) }),
        ];
        let hits_for_pred = Arc::clone(&hits);
        let pred = Arc::new(move || hits_for_pred.load(Ordering::SeqCst) >= 1);
        let calls = vec![
            ToolCallData { id: "1".into(), name: "a".into(), arguments: json!({}) },
            ToolCallData { id: "2".into(), name: "b".into(), arguments: json!({}) },
            ToolCallData { id: "3".into(), name: "c".into(), arguments: json!({}) },
        ];
        let emit: Emit = Arc::new(|_| {});
        let results = execute_tool_calls_for_test(
            calls,
            &tools,
            &Hooks::default(),
            CancellationToken::new(),
            &emit,
            true,
            None,
            Some(pred.as_ref()),
        )
        .await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "only first tool should run");
        assert_eq!(results.len(), 3);
        assert!(!results[0].is_error);
        assert!(results[1].is_error);
        assert!(results[1].model_text().contains("Skipped"));
        assert!(results[2].model_text().contains("Skipped"));
    }

    #[tokio::test]
    async fn thrash_blocks_identical_sequential_tools() {
        let hits = Arc::new(AtomicUsize::new(0));
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(CountTool {
            name: "bash".into(),
            hits: Arc::clone(&hits),
        })];
        let thrash = crate::thrash::ThrashGuard::with_limits(3, 10);
        let calls: Vec<_> = (0..4)
            .map(|i| ToolCallData {
                id: format!("{i}"),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            })
            .collect();
        let emit: Emit = Arc::new(|_| {});
        let results = execute_tool_calls_for_test(
            calls,
            &tools,
            &Hooks::default(),
            CancellationToken::new(),
            &emit,
            true,
            Some(&thrash),
            None,
        )
        .await;
        // First two run, third trips loop (max_repeats=3 means trip on 3rd observe)
        assert!(results.iter().any(|r| r.model_text().contains("loop detection") || r.model_text().contains("Skipped")));
        assert!(hits.load(Ordering::SeqCst) <= 3);
    }

    #[tokio::test]
    async fn thrash_blocks_identical_parallel_tools() {
        // Default Agent tool_execution is Parallel — thrash must still arm.
        let hits = Arc::new(AtomicUsize::new(0));
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(CountTool {
            name: "bash".into(),
            hits: Arc::clone(&hits),
        })];
        let thrash = crate::thrash::ThrashGuard::with_limits(3, 10);
        let calls: Vec<_> = (0..4)
            .map(|i| ToolCallData {
                id: format!("p{i}"),
                name: "bash".into(),
                arguments: json!({"command": "ls"}),
            })
            .collect();
        let emit: Emit = Arc::new(|_| {});
        let results = execute_tool_calls_for_test(
            calls,
            &tools,
            &Hooks::default(),
            CancellationToken::new(),
            &emit,
            false, // parallel
            Some(&thrash),
            None,
        )
        .await;
        assert!(
            results
                .iter()
                .any(|r| r.model_text().contains("loop detection")
                    || r.model_text().contains("thrash")),
            "parallel path must surface loop detection: {:?}",
            results.iter().map(|r| r.model_text()).collect::<Vec<_>>()
        );
        assert!(
            thrash.peek_stop().is_some()
                || results.iter().any(|r| r.model_text().contains("loop")),
            "thrash stop should be set after parallel identical signatures"
        );
    }

    #[tokio::test]
    async fn parallel_batch_honors_steer_skip_remaining() {
        let hits = Arc::new(AtomicUsize::new(0));
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(CountTool {
            name: "bash".into(),
            hits: Arc::clone(&hits),
        })];
        // First pred check is false (allow first tool); later checks true (skip).
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let pred = move || {
            let i = n2.fetch_add(1, Ordering::SeqCst);
            i >= 1
        };
        let calls: Vec<_> = (0..3)
            .map(|i| ToolCallData {
                id: format!("s{i}"),
                name: "bash".into(),
                arguments: json!({"command": "x"}),
            })
            .collect();
        let emit: Emit = Arc::new(|_| {});
        let results = execute_tool_calls_for_test(
            calls,
            &tools,
            &Hooks::default(),
            CancellationToken::new(),
            &emit,
            false, // parallel
            None,
            Some(&pred),
        )
        .await;
        assert_eq!(results.len(), 3);
        assert!(
            results[1].model_text().contains("Skipped")
                || results[2].model_text().contains("Skipped"),
            "parallel path must skip remaining on steer: {:?}",
            results.iter().map(|r| r.model_text()).collect::<Vec<_>>()
        );
        assert!(hits.load(Ordering::SeqCst) <= 1, "at most first tool runs");
    }
}
