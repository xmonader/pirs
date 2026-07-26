//! LLM streaming into context.
use std::sync::Arc;

use futures::stream::StreamExt;
use pirs_ai::{
    AssistantMessage, ContentBlock, Context, LlmProvider, Message, StopReason, StreamEvent,
};
use tokio_util::sync::CancellationToken;

use crate::events::{AgentEvent, Emit};
use crate::tool::{tool_defs, AgentTool};

use super::{is_visible, LoopConfig};


pub(super) async fn stream_assistant(
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
        // Credit rejected draft tokens before pop so cost accounting is honest (M-16).
        {
            let mut extra = config.extra_usage.lock().unwrap();
            *extra += draft.usage.clone();
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

pub(super) async fn stream_once(
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
    // A pin inserted at len-1 by a transform (or preserve_control_pins) can land
    // between a trailing assistant tool_use and its tool_result; repair adjacency
    // before serialization or the backend rejects the request (dangling tool_call).
    crate::control_pins::enforce_tool_result_adjacency(&mut messages);
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

pub(super) fn append_text(msg: &mut AssistantMessage, delta: String) {
    match msg.content.last_mut() {
        Some(ContentBlock::Text { text, .. }) => text.push_str(&delta),
        _ => msg.content.push(ContentBlock::text(delta)),
    }
}

pub(super) fn append_thinking(msg: &mut AssistantMessage, delta: String) {
    match msg.content.last_mut() {
        Some(ContentBlock::Thinking { thinking, .. }) => thinking.push_str(&delta),
        _ => msg.content.push(ContentBlock::Thinking {
            thinking: delta,
            thinking_signature: None,
            redacted: false,
        }),
    }
}

pub(super) fn replace_last(context: &mut Context, msg: &AssistantMessage) {
    if let Some(last) = context.messages.last_mut() {
        *last = Message::Assistant(msg.clone());
    }
}

/// O(1) delta application to the trailing assistant message in context —
/// avoids cloning the whole AssistantMessage on every streamed token.
pub(super) fn append_delta_to_last(context: &mut Context, delta: &str, thinking: bool) {
    if let Some(Message::Assistant(a)) = context.messages.last_mut() {
        if thinking {
            append_thinking(a, delta.to_string());
        } else {
            append_text(a, delta.to_string());
        }
    }
}


