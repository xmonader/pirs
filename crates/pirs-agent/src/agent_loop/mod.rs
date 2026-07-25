//! Agent tool-calling loop: stream → tool dispatch → result caps.

mod freeform;
mod stream;
mod tool_exec;

pub use freeform::looks_like_freeform_tool_text;
pub use tool_exec::validate_tool_call_payload;
pub use tool_exec::execute_tool_calls_for_test;

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

/// Cap tool result text fed back to the model (chars).
pub const MODEL_MAX_TOOL_RESULT_CHARS: usize = 20_000;

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
    let mut freeform_tool_nudge_sent = false;
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
                stream::stream_assistant(context, tools, provider, config, emit, cancel.clone()).await;
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
                let dangling = freeform::extract_tool_calls(&assistant);
                let mut tool_results = Vec::new();
                for call in &dangling {
                    let r = tool_exec::error_result_kind(
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

            let calls = freeform::extract_tool_calls(&assistant);
            let had_calls = !calls.is_empty();
            let mut results: Vec<ToolResultMessage> = Vec::new();
            // Weak models sometimes paste pseudo-tool calls as freeform text
            // (markdown fences / JSON arrays) with no native ToolCall blocks.
            // That is not successful tool use — re-prompt once for native tools.
            if !had_calls && !freeform_tool_nudge_sent {
                if let Some(nudge) = freeform::freeform_tool_repair_nudge(&assistant) {
                    freeform_tool_nudge_sent = true;
                    let msg = Message::user(nudge);
                    emit(AgentEvent::MessageStart {
                        message: Box::new(msg.clone()),
                    });
                    context.messages.push(msg.clone());
                    emit(AgentEvent::MessageEnd {
                        message: Box::new(msg.clone()),
                    });
                    new_messages.push(msg);
                    // Continue the turn loop so the model can re-issue properly.
                    turn_count += 1;
                    if config
                        .budgets
                        .max_turns
                        .map(|m| turn_count >= m)
                        .unwrap_or(false)
                    {
                        budget_hit = Some(BudgetHit::Turns);
                        emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        });
                        return (new_messages, budget_hit);
                    }
                    continue;
                }
            }
            if had_calls {
                if assistant.stop_reason == StopReason::Length {
                    for call in &calls {
                        results.push(tool_exec::error_result_kind(
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
                    results = tool_exec::execute_tool_calls(
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

#[cfg(test)]
#[path = "../agent_loop_result_cap_tests.rs"]
mod result_cap_tests;

#[cfg(test)]
#[path = "../agent_loop_skip_remaining_tests.rs"]
mod skip_remaining_tests;

#[cfg(test)]
#[path = "../agent_loop_tool_dispatch_tests.rs"]
mod tool_dispatch_tests;
