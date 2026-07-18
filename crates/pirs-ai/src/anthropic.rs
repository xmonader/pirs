use serde_json::{json, Value};

use crate::sse::SseEventStream;
use crate::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 8192;
const MAX_ERROR_BODY: usize = 4000;

pub struct AnthropicClient {
    base_url: String,
    client: reqwest::Client,
    max_retries: u32,
}

impl AnthropicClient {
    pub fn new(base_url: Option<String>) -> Self {
        AnthropicClient {
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            client: reqwest::Client::builder()
                .user_agent(concat!("pirs/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            max_retries: 0,
        }
    }

    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicClient {
    async fn stream(
        &self,
        model: &str,
        context: &Context,
        options: &CompletionOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::stream::BoxStream<'static, StreamEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(256);
        let client = self.client.clone();
        let url = format!("{}/v1/messages", self.base_url);
        let max_retries = self.max_retries;
        let body = build_request_body(model, context, options);
        let options = options.clone();
        let model_name = model.to_string();

        tokio::spawn(async move {
            let job = RequestJob {
                client,
                url,
                model: model_name,
                body,
                options,
                max_retries,
            };
            run_request(job, cancel, tx).await;
        });

        Box::pin(futures_util::stream::poll_fn(
            move |cx: &mut std::task::Context<'_>| rx.poll_recv(cx),
        ))
    }
}

struct RequestJob {
    client: reqwest::Client,
    url: String,
    model: String,
    body: Value,
    options: CompletionOptions,
    max_retries: u32,
}

async fn run_request(
    job: RequestJob,
    cancel: tokio_util::sync::CancellationToken,
    tx: tokio::sync::mpsc::Sender<StreamEvent>,
) {
    let RequestJob {
        client,
        url,
        model,
        body,
        options,
        max_retries,
    } = job;
    let mut stream_attempt = 0u32;
    let outcome = 'retry: loop {
        let response = {
            let mut req = client.post(&url).json(&body);
            if let Some(key) = &options.api_key {
                req = req.header("x-api-key", key);
            }
            req = req.header("anthropic-version", ANTHROPIC_VERSION);
            for (k, v) in &options.extra_headers {
                req = req.header(k, v);
            }
            if let Some(t) = options.timeout {
                req = req.timeout(t);
            }
            let result = tokio::select! {
                _ = cancel.cancelled() => {
                    send_terminal(&tx, &model, StopReason::Aborted, None).await;
                    return;
                }
                r = req.send() => r,
            };
            match result {
                Ok(resp) if resp.status().is_success() => resp,
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    let body_text = resp.text().await.unwrap_or_default();
                    if (status == 429 || status >= 500) && stream_attempt < max_retries {
                        stream_attempt += 1;
                        backoff(stream_attempt, retry_after, &cancel).await;
                        continue 'retry;
                    }
                    let msg = extract_error_body(&body_text);
                    send_terminal(&tx, &model, StopReason::Error, Some(msg)).await;
                    return;
                }
                Err(e) => {
                    if stream_attempt < max_retries {
                        stream_attempt += 1;
                        backoff(stream_attempt, None, &cancel).await;
                        continue 'retry;
                    }
                    send_terminal(&tx, &model, StopReason::Error, Some(e.to_string())).await;
                    return;
                }
            }
        };

        let outcome = stream_response(response, &model, &cancel, &tx).await;
        let empty = outcome.message.content.is_empty()
            || outcome.message.content.iter().all(|b| match b {
                ContentBlock::Text { text, .. } => text.trim().is_empty(),
                ContentBlock::Thinking { thinking, .. } => thinking.trim().is_empty(),
                _ => false,
            });
        let retryable = matches!(outcome.message.stop_reason, StopReason::Error)
            || (empty && matches!(outcome.message.stop_reason, StopReason::Stop));
        if retryable
            && !outcome.deltas_sent
            && stream_attempt < max_retries
            && !cancel.is_cancelled()
        {
            stream_attempt += 1;
            tracing::warn!(
                "retrying completion (attempt {stream_attempt}/{max_retries}): {}",
                outcome
                    .message
                    .error_message
                    .as_deref()
                    .unwrap_or("empty completion")
            );
            backoff(stream_attempt, None, &cancel).await;
            continue 'retry;
        }
        break outcome;
    };

    let _ = tx.send(StreamEvent::Done(Box::new(outcome.message))).await;
}

struct StreamOutcome {
    message: AssistantMessage,
    deltas_sent: bool,
}

async fn backoff(
    attempt: u32,
    retry_after: Option<u64>,
    cancel: &tokio_util::sync::CancellationToken,
) {
    let secs = retry_after.unwrap_or_else(|| 1u64 << attempt.min(5));
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
    }
}

async fn send_terminal(
    tx: &tokio::sync::mpsc::Sender<StreamEvent>,
    model: &str,
    reason: StopReason,
    error: Option<String>,
) {
    let msg = AssistantMessage {
        provider: "anthropic".to_string(),
        api: "anthropic-messages".to_string(),
        model: model.to_string(),
        stop_reason: reason,
        error_message: error,
        ..Default::default()
    };
    let _ = tx.send(StreamEvent::Done(Box::new(msg))).await;
}

fn extract_error_body(body: &str) -> String {
    let truncated: String = body.chars().take(MAX_ERROR_BODY).collect();
    if let Ok(v) = serde_json::from_str::<Value>(&truncated) {
        if let Some(err) = v.get("error") {
            if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                return m.to_string();
            }
            return err.to_string();
        }
    }
    truncated
}

#[derive(Default)]
struct BlockAcc {
    kind: String,
    text: String,
    thinking: String,
    signature: String,
    tool_id: String,
    tool_name: String,
    tool_input_json: String,
}

#[derive(Default)]
struct Accumulator {
    blocks: Vec<BlockAcc>,
    input_tokens: u64,
    cache_read: u64,
    cache_write: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
}

impl Accumulator {
    fn block_mut(&mut self, index: usize) -> &mut BlockAcc {
        if self.blocks.len() <= index {
            self.blocks.resize_with(index + 1, BlockAcc::default);
        }
        &mut self.blocks[index]
    }

    /// Returns (events, deltas_happened)
    fn apply(&mut self, event: &str, data: &Value) -> (Vec<StreamEvent>, bool) {
        let mut events = Vec::new();
        let mut deltas = false;
        match event {
            "message_start" => {
                if let Some(u) = data.pointer("/message/usage") {
                    self.input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.cache_read = u
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    self.cache_write = u
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            "content_block_start" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let kind = data
                    .pointer("/content_block/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("text")
                    .to_string();
                let block = self.block_mut(index);
                block.kind = kind.clone();
                if kind == "redacted_thinking" {
                    block.thinking = data
                        .pointer("/content_block/data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
                if kind == "tool_use" {
                    block.tool_id = data
                        .pointer("/content_block/id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    block.tool_name = data
                        .pointer("/content_block/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let delta_type = data
                    .pointer("/delta/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(t) = data.pointer("/delta/text").and_then(|v| v.as_str()) {
                            self.block_mut(index).text.push_str(t);
                            events.push(StreamEvent::TextDelta(t.to_string()));
                            deltas = true;
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = data.pointer("/delta/thinking").and_then(|v| v.as_str()) {
                            self.block_mut(index).thinking.push_str(t);
                            events.push(StreamEvent::ThinkingDelta(t.to_string()));
                            deltas = true;
                        }
                    }
                    "signature_delta" => {
                        if let Some(s) = data.pointer("/delta/signature").and_then(|v| v.as_str()) {
                            self.block_mut(index).signature.push_str(s);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(p) =
                            data.pointer("/delta/partial_json").and_then(|v| v.as_str())
                        {
                            self.block_mut(index).tool_input_json.push_str(p);
                            events.push(StreamEvent::ToolCallDelta);
                            deltas = true;
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(sr) = data.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(u) = data.get("usage") {
                    self.output_tokens =
                        u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
            "error" => {
                let msg = extract_error_body(&data.to_string());
                events.push(StreamEvent::Error(msg));
            }
            _ => {}
        }
        (events, deltas)
    }

    fn into_message(self, model: &str) -> AssistantMessage {
        let mut content = Vec::new();
        for block in self.blocks {
            match block.kind.as_str() {
                "redacted_thinking" => {
                    content.push(ContentBlock::Thinking {
                        thinking: block.thinking,
                        thinking_signature: None,
                        redacted: true,
                    });
                }
                "thinking" => {
                    if !block.thinking.is_empty() || block.kind == "redacted_thinking" {
                        content.push(ContentBlock::Thinking {
                            thinking: block.thinking,
                            thinking_signature: if block.signature.is_empty() {
                                None
                            } else {
                                Some(block.signature)
                            },
                            redacted: block.kind == "redacted_thinking",
                        });
                    }
                }
                "tool_use" => {
                    let arguments = if block.tool_input_json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&block.tool_input_json)
                            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
                    };
                    content.push(ContentBlock::ToolCall {
                        id: block.tool_id,
                        name: block.tool_name,
                        arguments,
                        thought_signature: None,
                    });
                }
                _ => {
                    if !block.text.is_empty() {
                        content.push(ContentBlock::Text {
                            text: block.text,
                            text_signature: None,
                        });
                    }
                }
            }
        }
        let stop_reason = map_stop_reason(self.stop_reason.as_deref());
        AssistantMessage {
            content,
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: model.to_string(),
            usage: Usage {
                input: self.input_tokens,
                output: self.output_tokens,
                cache_read: self.cache_read,
                cache_write: self.cache_write,
                total_tokens: self.input_tokens + self.output_tokens,
                reasoning: 0,
            },
            stop_reason,
            ..Default::default()
        }
    }
}

pub fn map_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") | Some("") | None => StopReason::Stop,
        Some("max_tokens") => StopReason::Length,
        Some("tool_use") => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

async fn stream_response(
    response: reqwest::Response,
    model: &str,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> StreamOutcome {
    let mut sse = SseEventStream::new(response);
    let mut acc = Accumulator::default();
    let mut deltas_sent = false;
    let _ = tx.send(StreamEvent::Start).await;
    let mut saw_stop = false;

    loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => {
                return StreamOutcome {
                    message: aborted_message(model),
                    deltas_sent,
                };
            }
            i = sse.next() => i,
        };
        let item = match item {
            None => break,
            Some(Ok(i)) => i,
            Some(Err(e)) => {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                return StreamOutcome {
                    message: error_message(model, e.to_string()),
                    deltas_sent,
                };
            }
        };
        let (event, payload) = match parse_sse_event(&item) {
            Some(v) => v,
            None => continue,
        };
        if event == "message_stop" {
            saw_stop = true;
            break;
        }
        if event == "error" {
            let msg = extract_error_body(&payload.to_string());
            let _ = tx.send(StreamEvent::Error(msg.clone())).await;
            return StreamOutcome {
                message: error_message(model, msg),
                deltas_sent,
            };
        }
        let (events, deltas) = acc.apply(&event, &payload);
        deltas_sent |= deltas;
        for ev in events {
            let _ = tx.send(ev).await;
        }
    }

    if !saw_stop && acc.stop_reason.is_none() {
        let msg = "Stream ended without message_stop".to_string();
        let _ = tx.send(StreamEvent::Error(msg.clone())).await;
        return StreamOutcome {
            message: error_message(model, msg),
            deltas_sent,
        };
    }
    // Tolerate servers that close right after message_delta (message_stop
    // is the spec terminator, but a clean stop_reason is enough).

    StreamOutcome {
        message: acc.into_message(model),
        deltas_sent,
    }
}

fn parse_sse_event(item: &crate::sse::SseEvent) -> Option<(String, Value)> {
    let v: Value = serde_json::from_str(&item.data).ok()?;
    let event = item.event.clone().or_else(|| {
        v.get("type")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    })?;
    Some((event, v))
}

fn aborted_message(model: &str) -> AssistantMessage {
    AssistantMessage {
        provider: "anthropic".to_string(),
        api: "anthropic-messages".to_string(),
        model: model.to_string(),
        stop_reason: StopReason::Aborted,
        ..Default::default()
    }
}

fn error_message(model: &str, error: String) -> AssistantMessage {
    AssistantMessage {
        provider: "anthropic".to_string(),
        api: "anthropic-messages".to_string(),
        model: model.to_string(),
        stop_reason: StopReason::Error,
        error_message: Some(error),
        ..Default::default()
    }
}

pub fn build_request_body(model: &str, ctx: &Context, options: &CompletionOptions) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages_to_anthropic(ctx),
        "stream": true,
    });
    if let Some(effort) = &options.reasoning_effort {
        if effort != "off" {
            let budget = match effort.as_str() {
                "minimal" => 1024,
                "low" => 2048,
                "medium" => 8192,
                "high" => 16384,
                _ => 32768,
            };
            let max_out = options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
            let budget = budget.min(max_out.saturating_sub(1024).max(1024));
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        }
    }
    if let Some(sp) = &ctx.system_prompt {
        if !sp.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": sp,
                "cache_control": { "type": "ephemeral" }
            }]);
        }
    }
    if !ctx.tools.is_empty() {
        body["tools"] = Value::Array(
            ctx.tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }
    match options.tool_choice {
        Some(crate::ToolChoice::Auto) => body["tool_choice"] = json!({"type": "auto"}),
        Some(crate::ToolChoice::None) => body["tool_choice"] = json!({"type": "none"}),
        Some(crate::ToolChoice::Required) => body["tool_choice"] = json!({"type": "any"}),
        None => {}
    }
    body
}

pub fn messages_to_anthropic(ctx: &Context) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending_results: Vec<Value> = Vec::new();
    let total = ctx.messages.len();

    for msg in &ctx.messages {
        match msg {
            Message::ToolResult(tr) => {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
                pending_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tr.tool_call_id,
                    "content": if text.is_empty() { "(no tool output)".to_string() } else { text },
                    "is_error": tr.is_error,
                }));
            }
            Message::User(u) => {
                flush_results(&mut out, &mut pending_results);
                let is_last = {
                    let idx = out.len() + pending_results.len() + 1;
                    idx == total - 1
                };
                match &u.content {
                    crate::UserContent::Text(t) => {
                        out.push(json!({ "role": "user", "content": t }));
                    }
                    crate::UserContent::Blocks(blocks) => {
                        let parts: Vec<Value> = blocks
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text, .. } => {
                                    Some(json!({ "type": "text", "text": text }))
                                }
                                ContentBlock::Image { data, mime_type } => Some(json!({
                                    "type": "image",
                                    "source": { "type": "base64", "media_type": mime_type, "data": data }
                                })),
                                _ => None,
                            })
                            .collect();
                        if !parts.is_empty() {
                            out.push(json!({ "role": "user", "content": parts }));
                        }
                    }
                }
            }
            Message::Assistant(a) => {
                flush_results(&mut out, &mut pending_results);
                let mut parts: Vec<Value> = Vec::new();
                for b in &a.content {
                    match b {
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } => {
                            if *redacted {
                                parts
                                    .push(json!({ "type": "redacted_thinking", "data": thinking }));
                            } else {
                                let mut part = json!({ "type": "thinking", "thinking": thinking });
                                if let Some(sig) = thinking_signature {
                                    part["signature"] = json!(sig);
                                }
                                parts.push(part);
                            }
                        }
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => {
                            parts.push(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": arguments,
                            }));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(json!({ "role": "assistant", "content": parts }));
                }
            }
        }
    }
    flush_results(&mut out, &mut pending_results);
    // Cache breakpoint on the tail of the conversation so history is reused.
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some("user") {
            if let Some(content) = last.get_mut("content") {
                if let Some(s) = content.as_str() {
                    *content = serde_json::json!([{
                        "type": "text",
                        "text": s,
                        "cache_control": { "type": "ephemeral" }
                    }]);
                }
            }
        }
    }
    let _ = total;
    out
}

fn flush_results(out: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if !pending.is_empty() {
        out.push(json!({ "role": "user", "content": pending.clone() }));
        pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolResultMessage, UserMessage};

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason(Some("end_turn")), StopReason::Stop);
        assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::Length);
        assert_eq!(map_stop_reason(Some("tool_use")), StopReason::ToolUse);
        assert_eq!(map_stop_reason(Some("other")), StopReason::Error);
    }

    #[test]
    fn tool_results_bundle_into_one_user_message() {
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![
                Message::Assistant(AssistantMessage {
                    content: vec![
                        ContentBlock::ToolCall {
                            id: "t1".into(),
                            name: "bash".into(),
                            arguments: json!({"command": "ls"}),
                            thought_signature: None,
                        },
                        ContentBlock::ToolCall {
                            id: "t2".into(),
                            name: "read".into(),
                            arguments: json!({"path": "f"}),
                            thought_signature: None,
                        },
                    ],
                    ..Default::default()
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "t1".into(),
                    tool_name: "bash".into(),
                    content: vec![ContentBlock::text("files")],
                    details: None,
                    is_error: false,
                    terminate: false,
                    timestamp: 0,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "t2".into(),
                    tool_name: "read".into(),
                    content: vec![ContentBlock::text("content")],
                    details: None,
                    is_error: false,
                    terminate: false,
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: crate::UserContent::Text("next".into()),
                    timestamp: 0,
                }),
            ],
            tools: vec![],
        };
        let msgs = messages_to_anthropic(&ctx);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], "t1");
        assert_eq!(msgs[2]["role"], "user");
        let body = build_request_body("claude-x", &ctx, &CompletionOptions::default());
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn accumulator_full_stream() {
        let mut acc = Accumulator::default();
        let events = vec![
            (
                "message_start",
                json!({"message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 40}}}),
            ),
            (
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "thinking"}}),
            ),
            (
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm "}}),
            ),
            (
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "yes"}}),
            ),
            (
                "content_block_start",
                json!({"index": 1, "content_block": {"type": "text"}}),
            ),
            (
                "content_block_delta",
                json!({"index": 1, "delta": {"type": "text_delta", "text": "Let me "}}),
            ),
            (
                "content_block_start",
                json!({"index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "bash"}}),
            ),
            (
                "content_block_delta",
                json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
            ),
            (
                "content_block_delta",
                json!({"index": 2, "delta": {"type": "input_json_delta", "partial_json": " \"ls\"}"}}),
            ),
            ("content_block_stop", json!({"index": 2})),
            (
                "message_delta",
                json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 25}}),
            ),
        ];
        for (e, d) in &events {
            acc.apply(e, d);
        }
        let msg = acc.into_message("claude-x");
        assert_eq!(msg.stop_reason, StopReason::ToolUse);
        assert_eq!(msg.usage.input, 100);
        assert_eq!(msg.usage.cache_read, 40);
        assert_eq!(msg.usage.output, 25);
        assert!(
            matches!(&msg.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "hmm yes")
        );
        assert_eq!(msg.text(), "Let me ");
        match &msg.content[2] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "bash");
                assert_eq!(arguments["command"], "ls");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn parse_event_prefers_event_line_then_payload_type() {
        let item = crate::sse::SseEvent {
            event: Some("message_stop".into()),
            data: "{}".into(),
        };
        let (event, _) = parse_sse_event(&item).unwrap();
        assert_eq!(event, "message_stop");

        let item2 = crate::sse::SseEvent {
            event: None,
            data: r#"{"type":"ping"}"#.into(),
        };
        assert_eq!(parse_sse_event(&item2).unwrap().0, "ping");

        let item3 = crate::sse::SseEvent {
            event: None,
            data: "not json".into(),
        };
        assert!(parse_sse_event(&item3).is_none());
    }
}
