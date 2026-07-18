use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use pirs_ai::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent, ToolResultMessage,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compaction::{compact_messages, last_input_tokens, should_compact, CompactionConfig};
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

pub type CascadeJudge = Arc<
    dyn Fn(&AssistantMessage) -> futures::future::BoxFuture<'static, bool> + Send + Sync,
>;

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
            context
                .messages
                .push(Message::Assistant(assistant.clone()));
            new_messages.push(Message::Assistant(assistant.clone()));

            if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                emit(AgentEvent::TurnEnd {
                    message: Box::new(assistant),
                    tool_results: vec![],
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
                        results.push(error_result(
                            &call.id,
                            &call.name,
                            "Tool call arguments were truncated due to token limit. Re-issue the tool call.",
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
                    )
                    .await;
                }
                for r in &results {
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
                if last_input_tokens(&context.messages)
                    .map(|t| should_compact(t, cfg))
                    .unwrap_or(false)
                {
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

            let batch_terminate =
                !results.is_empty() && results.iter().all(|r| r.terminate);
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
        emit(AgentEvent::MessageStart {
            message: Box::new(Message::Assistant(AssistantMessage::default())),
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

    let mut messages = context.messages.clone();
    if let Some(t) = &config.hooks.transform_context {
        messages = t(messages);
    }
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
                append_text(&mut partial, d);
                replace_last(context, &partial);
                emit(AgentEvent::MessageUpdate {
                    message: Box::new(partial.clone()),
                });
            }
            StreamEvent::ThinkingDelta(d) => {
                append_thinking(&mut partial, d);
                replace_last(context, &partial);
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

fn error_result(id: &str, name: &str, message: &str) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        content: vec![ContentBlock::text(message)],
        details: None,
        is_error: true,
        terminate: false,
        timestamp: pirs_ai::now_millis(),
    }
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
            let req = if required.contains(&k.as_str()) { " (required)" } else { "" };
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
    let Some(tool) = tools.iter().find(|t| t.name() == call.name) else {
        return Prepared::Failed {
            index,
            result: error_result(&call.id, &call.name, &format!("Tool {} not found", call.name)),
        };
    };
    if !is_visible(visible, &call.name) {
        return Prepared::Failed {
            index,
            result: error_result(
                &call.id,
                &call.name,
                &format!(
                    "Tool {} is not loaded in this session. Call use_tool(\"{}\") first to load it, then re-issue your call.",
                    call.name, call.name
                ),
            ),
        };
    }
    let schema = tool.parameters();
    let args = coerce_args(&schema, &call.arguments);
    if let Err(e) = validate_args(&schema, &args) {
        return Prepared::Failed {
            index,
            result: error_result(
                &call.id,
                &call.name,
                &format!(
                    "Invalid arguments for tool {}: {e}. Expected: {}. Re-issue the call with corrected arguments.",
                    call.name,
                    schema_summary(&schema)
                ),
            ),
        };
    }
    if let Some(before) = &hooks.before_tool_call {
        if let Some(reason) = before(&call.id, &call.name, &args) {
            return Prepared::Failed {
                index,
                result: error_result(
                    &call.id,
                    &call.name,
                    &format!("Tool call blocked: {reason}"),
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
        Ok(out) => ToolResultMessage {
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
        },
        Err(e) => error_result(id, name, &e.to_string()),
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
    result
}

async fn execute_tool_calls(
    calls: Vec<ToolCallData>,
    tools: &[Arc<dyn AgentTool>],
    hooks: &Hooks,
    cancel: CancellationToken,
    emit: &Emit,
    sequential: bool,
    visible: Option<VisibleTools>,
) -> Vec<ToolResultMessage> {
    let n = calls.len();
    let meta: Vec<(String, String)> = calls
        .iter()
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect();
    let mut results: Vec<Option<ToolResultMessage>> = Vec::with_capacity(n);
    results.resize_with(n, || None);

    if sequential {
        for (index, call) in calls.into_iter().enumerate() {
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
                    let outcome = run_tool(tool, id.clone(), name.clone(), args, cancel.clone(), emit).await;
                    finalize_result(&id, &name, outcome, hooks)
                }
            };
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: result.tool_call_id.clone(),
                tool_name: result.tool_name.clone(),
                result: Box::new(result.clone()),
            });
            results[index] = Some(result);
            if cancel.is_cancelled() {
                break;
            }
        }
    } else {
        let mut prepared = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            emit(AgentEvent::ToolExecutionStart {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                args: call.arguments.clone(),
            });
            match prepare_call(index, &call, tools, hooks, &visible) {
                Prepared::Failed { index, result } => {
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
        let mut in_flight = FuturesUnordered::new();
        for p in prepared {
            if let Prepared::Ready {
                index,
                id,
                name,
                args,
                tool,
            } = p
            {
                let cancel = cancel.clone();
                in_flight.push(async move {
                    let outcome =
                        run_tool(tool, id.clone(), name.clone(), args, cancel, emit).await;
                    (index, id, name, outcome)
                });
            }
        }
        while let Some((index, id, name, outcome)) = in_flight.next().await {
            let result = finalize_result(&id, &name, outcome, hooks);
            emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: id,
                tool_name: name,
                result: Box::new(result.clone()),
            });
            results[index] = Some(result);
        }
    }

    results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            r.unwrap_or_else(|| {
                let (id, name) = &meta[i];
                error_result(id, name, "Tool execution did not complete")
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
        tool_call_id: id,
        args,
        cancel,
        on_update: Some(on_update),
    };
    tool.execute(ctx).await
}
