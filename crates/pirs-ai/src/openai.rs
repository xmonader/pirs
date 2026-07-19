use serde_json::{json, Value};

use crate::sse::SseStream;
use crate::{
    AiError, AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message,
    StopReason, StreamEvent, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Upper bound on streamed tool calls in one response; clamps a hostile
/// `index` before it can overflow index+1 or trigger a huge allocation.
const MAX_TOOL_CALLS: usize = 4096;

pub struct OpenAiCompat {
    base_url: String,
    provider_name: String,
    client: reqwest::Client,
    max_retries: u32,
    cache_key: Option<String>,
}

impl OpenAiCompat {
    pub fn new(base_url: Option<String>) -> Self {
        OpenAiCompat {
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            provider_name: "openai".to_string(),
            client: reqwest::Client::builder()
                .user_agent(concat!("pirs/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            max_retries: 0,
            cache_key: None,
        }
    }

    /// Sent as prompt_cache_key, but only to api.openai.com (other gateways
    /// may reject unknown fields — same gating as pi).
    pub fn with_cache_key(mut self, key: impl Into<String>) -> Self {
        if self.base_url.contains("api.openai.com") {
            self.cache_key = Some(key.into());
        }
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiCompat {
    async fn stream(
        &self,
        model: &str,
        context: &Context,
        options: &CompletionOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::stream::BoxStream<'static, StreamEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(256);
        let client = self.client.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let provider = self.provider_name.clone();
        let max_retries = self.max_retries;
        let mut body = build_request_body(model, context, options);
        if let Some(key) = &self.cache_key {
            body["prompt_cache_key"] = serde_json::Value::String(key.clone());
        }
        let options = options.clone();
        let model_name = model.to_string();

        tokio::spawn(async move {
            let job = RequestJob {
                client,
                url,
                provider,
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
    provider: String,
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
        provider,
        model,
        body,
        options,
        max_retries,
    } = job;
    let cache_affinity = if url.contains("mistral.ai") {
        Some(format!("pirs-{}", std::process::id()))
    } else {
        None
    };
    let mut stream_attempt = 0u32;
    let mut attempt = 0u32;
    let outcome = 'retry: loop {
        let response = {
            let response = loop {
                let mut req = client.post(&url).json(&body);
                let has_auth_override = options
                    .extra_headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                if let Some(key) = &options.api_key {
                    if !has_auth_override {
                        req = req.bearer_auth(key);
                    }
                }
                if url.contains("mistral.ai") {
                    if let Some(affinity) = &cache_affinity {
                        req = req.header("x-affinity", affinity.clone());
                    }
                }
                for (k, v) in &options.extra_headers {
                    req = req.header(k, v);
                }
                if let Some(t) = options.timeout {
                    req = req.timeout(t);
                }

                let result = tokio::select! {
                    _ = cancel.cancelled() => {
                        send_done(&tx, &provider, &model, StopReason::Aborted, None).await;
                        return;
                    }
                    r = req.send() => r,
                };

                match result {
                    Ok(resp) if resp.status().is_success() => break resp,
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let retry_after = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());
                        let body_text = resp.text().await.unwrap_or_default();
                        if (status == 429 || status >= 500) && attempt < max_retries {
                            attempt += 1;
                            backoff(attempt, retry_after, &cancel).await;
                            continue;
                        }
                        let msg = extract_error_body(&body_text);
                        send_done(&tx, &provider, &model, StopReason::Error, Some(msg)).await;
                        return;
                    }
                    Err(e) => {
                        if attempt < max_retries {
                            attempt += 1;
                            backoff(attempt, None, &cancel).await;
                            continue;
                        }
                        let _ = tx
                            .send(StreamEvent::Error(AiError::Network(e).to_string()))
                            .await;
                        send_done(&tx, &provider, &model, StopReason::Error, None).await;
                        return;
                    }
                }
            };
            response
        };

        let outcome = stream_response(response, &provider, &model, &cancel, &tx).await;
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

use crate::retry::backoff;

async fn send_done(
    tx: &tokio::sync::mpsc::Sender<StreamEvent>,
    provider: &str,
    model: &str,
    reason: StopReason,
    error: Option<String>,
) {
    let msg = AssistantMessage {
        provider: provider.to_string(),
        api: "openai-completions".to_string(),
        model: model.to_string(),
        stop_reason: reason,
        error_message: error,
        ..Default::default()
    };
    let _ = tx.send(StreamEvent::Done(Box::new(msg))).await;
}

use crate::retry::extract_error_body;

async fn stream_response(
    response: reqwest::Response,
    provider: &str,
    model: &str,
    cancel: &tokio_util::sync::CancellationToken,
    tx: &tokio::sync::mpsc::Sender<StreamEvent>,
) -> StreamOutcome {
    let mut sse = SseStream::new(response);
    let mut acc = Accumulator::default();
    let mut deltas_sent = false;
    let _ = tx.send(StreamEvent::Start).await;

    loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => {
                return StreamOutcome {
                    message: aborted_message(provider, model),
                    deltas_sent,
                };
            }
            i = sse.next() => i,
        };
        let data = match item {
            None => break,
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                return StreamOutcome {
                    message: error_message(provider, model, e.to_string()),
                    deltas_sent,
                };
            }
        };
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(err) = chunk.get("error") {
            let msg = extract_error_body(&err.to_string());
            let _ = tx.send(StreamEvent::Error(msg.clone())).await;
            return StreamOutcome {
                message: error_message(provider, model, msg),
                deltas_sent,
            };
        }
        for ev in acc.apply_chunk(&chunk) {
            if matches!(
                ev,
                StreamEvent::TextDelta(_)
                    | StreamEvent::ThinkingDelta(_)
                    | StreamEvent::ToolCallDelta
            ) {
                deltas_sent = true;
            }
            let _ = tx.send(ev).await;
        }
    }

    if acc.finish_reason.is_none() {
        let msg = "Stream ended without finish_reason".to_string();
        let _ = tx.send(StreamEvent::Error(msg.clone())).await;
        return StreamOutcome {
            message: error_message(provider, model, msg),
            deltas_sent,
        };
    }

    StreamOutcome {
        message: acc.into_message(provider, model),
        deltas_sent,
    }
}

fn aborted_message(provider: &str, model: &str) -> AssistantMessage {
    AssistantMessage {
        provider: provider.to_string(),
        api: "openai-completions".to_string(),
        model: model.to_string(),
        stop_reason: StopReason::Aborted,
        ..Default::default()
    }
}

fn error_message(provider: &str, model: &str, error: String) -> AssistantMessage {
    AssistantMessage {
        provider: provider.to_string(),
        api: "openai-completions".to_string(),
        model: model.to_string(),
        stop_reason: StopReason::Error,
        error_message: Some(error),
        ..Default::default()
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct Accumulator {
    text: String,
    thinking: String,
    tool_calls: Vec<PartialToolCall>,
    usage: Usage,
    finish_reason: Option<String>,
}

impl Accumulator {
    fn apply_chunk(&mut self, chunk: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(usage);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        else {
            return events;
        };

        if let Some(usage) = choice.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(usage);
        }

        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if !fr.is_empty() {
                self.finish_reason = Some(fr.to_string());
            }
        }

        let Some(delta) = choice.get("delta") else {
            return events;
        };

        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                self.text.push_str(content);
                events.push(StreamEvent::TextDelta(content.to_string()));
            }
        }

        let reasoning = ["reasoning_content", "reasoning", "reasoning_text"]
            .iter()
            .find_map(|k| delta.get(k).and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty());
        if let Some(r) = reasoning {
            self.thinking.push_str(r);
            events.push(StreamEvent::ThinkingDelta(r.to_string()));
        }

        if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for call in calls {
                self.apply_tool_call(call);
                events.push(StreamEvent::ToolCallDelta);
            }
        }

        events
    }

    fn apply_tool_call(&mut self, call: &Value) {
        let id = call.get("id").and_then(|v| v.as_str());
        // Clamp: a hostile `index` (up to u64::MAX) must not overflow index+1
        // and panic, nor allocate gigabytes. Real responses stay tiny.
        let index = (call.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize)
            .min(MAX_TOOL_CALLS - 1);

        let slot = if self.tool_calls.len() > index {
            index
        } else {
            self.tool_calls
                .resize_with(index + 1, PartialToolCall::default);
            index
        };
        let entry = &mut self.tool_calls[slot];
        let _ = id;

        if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
            entry.id = normalize_tool_call_id(id);
        }
        if let Some(f) = call.get("function") {
            if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                entry.name.push_str(name);
            }
            if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                entry.args.push_str(args);
            }
        }
    }

    fn into_message(self, provider: &str, model: &str) -> AssistantMessage {
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: self.thinking,
                thinking_signature: None,
                redacted: false,
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text,
                text_signature: None,
            });
        }
        for call in self.tool_calls {
            let arguments = serde_json::from_str(&call.args)
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
            content.push(ContentBlock::ToolCall {
                id: call.id,
                name: call.name,
                arguments,
                thought_signature: None,
            });
        }
        let stop_reason = map_finish_reason(self.finish_reason.as_deref());
        AssistantMessage {
            content,
            api: "openai-completions".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            usage: self.usage,
            stop_reason,
            ..Default::default()
        }
    }
}

pub fn normalize_tool_call_id(id: &str) -> String {
    let stripped = id.split('|').next().unwrap_or(id);
    let sanitized: String = stripped
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(40).collect()
}

pub fn map_finish_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") | Some("end") | Some("") | None => StopReason::Stop,
        Some("length") => StopReason::Length,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

fn parse_usage(v: &Value) -> Usage {
    let prompt = v.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let completion = v
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cache_read = v
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let reasoning = v
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    Usage {
        input: prompt.saturating_sub(cache_read),
        output: completion,
        cache_read,
        cache_write: 0,
        total_tokens: prompt + completion,
        reasoning,
    }
}

pub fn build_request_body(model: &str, ctx: &Context, options: &CompletionOptions) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages_to_openai(ctx),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = options.max_tokens {
        body["max_completion_tokens"] = json!(m);
    }
    if !ctx.tools.is_empty() {
        body["tools"] = Value::Array(ctx.tools.iter().map(tool_to_openai).collect());
    }
    match options.tool_choice {
        Some(crate::ToolChoice::Auto) => body["tool_choice"] = json!("auto"),
        Some(crate::ToolChoice::None) => body["tool_choice"] = json!("none"),
        Some(crate::ToolChoice::Required) => body["tool_choice"] = json!("required"),
        None => {}
    }
    if let Some(effort) = &options.reasoning_effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

fn tool_to_openai(tool: &crate::ToolDef) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
            "strict": false,
        }
    })
}

fn echo_reasoning_to_provider(provider: &str) -> bool {
    // DeepSeek REQUIRES reasoning_content echoed back; most OpenAI-compat
    // gateways reject it in input. Echo only to known-safe providers.
    matches!(provider, "deepseek")
}

pub fn messages_to_openai(ctx: &Context) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if let Some(sp) = &ctx.system_prompt {
        if !sp.is_empty() {
            out.push(json!({ "role": "system", "content": sp }));
        }
    }
    for msg in &ctx.messages {
        match msg {
            Message::User(u) => match &u.content {
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
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime_type};base64,{data}") }
                            })),
                            _ => None,
                        })
                        .collect();
                    if !parts.is_empty() {
                        out.push(json!({ "role": "user", "content": parts }));
                    }
                }
            },
            Message::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                let thinking: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<Value> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(arguments).unwrap_or_default(),
                            }
                        })),
                        _ => None,
                    })
                    .collect();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut m = json!({ "role": "assistant" });
                if calls.is_empty() {
                    m["content"] = json!(text);
                } else if text.is_empty() {
                    m["content"] = Value::Null;
                } else {
                    m["content"] = json!(text);
                }
                if !calls.is_empty() {
                    m["tool_calls"] = Value::Array(calls);
                }
                if !thinking.is_empty() && echo_reasoning_to_provider(&a.provider) {
                    m["reasoning_content"] = json!(thinking);
                }
                out.push(m);
            }
            Message::ToolResult(tr) => {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id,
                    "content": if text.is_empty() { "(no tool output)".to_string() } else { text },
                }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolResultMessage, UserMessage};

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason(Some("stop")), StopReason::Stop);
        assert_eq!(map_finish_reason(Some("length")), StopReason::Length);
        assert_eq!(map_finish_reason(Some("tool_calls")), StopReason::ToolUse);
        assert_eq!(map_finish_reason(Some("content_filter")), StopReason::Error);
    }

    #[test]
    fn tool_call_id_normalized() {
        assert_eq!(normalize_tool_call_id("call_abc|xyz"), "call_abc");
        assert_eq!(normalize_tool_call_id("a b.c"), "a_b_c");
        assert_eq!(normalize_tool_call_id(&"x".repeat(50)), "x".repeat(40));
    }

    #[test]
    fn assistant_with_tool_calls_serializes() {
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![
                Message::User(UserMessage {
                    content: crate::UserContent::Text("hi".into()),
                    timestamp: 0,
                }),
                Message::Assistant(AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls"}),
                        thought_signature: None,
                    }],
                    ..Default::default()
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1".into(),
                    tool_name: "bash".into(),
                    content: vec![ContentBlock::text("file.txt")],
                    details: None,
                    is_error: false,
                    terminate: false,
                    timestamp: 0,
                }),
            ],
            tools: vec![],
        };
        let msgs = messages_to_openai(&ctx);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[2]["role"], "assistant");
        assert!(msgs[2]["content"].is_null());
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["content"], "file.txt");
    }

    #[test]
    fn empty_assistant_skipped_and_empty_tool_result_placeholder() {
        let ctx = Context {
            system_prompt: None,
            messages: vec![
                Message::Assistant(AssistantMessage::default()),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "t".into(),
                    tool_name: "x".into(),
                    content: vec![],
                    details: None,
                    is_error: false,
                    terminate: false,
                    timestamp: 0,
                }),
            ],
            tools: vec![],
        };
        let msgs = messages_to_openai(&ctx);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"], "(no tool output)");
    }

    #[test]
    fn accumulator_collects_stream() {
        let mut acc = Accumulator::default();
        let chunks = vec![
            json!({"choices":[{"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"bash","arguments":"{\"com"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mand\":\"ls\"}"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
        ];
        for c in &chunks {
            acc.apply_chunk(c);
        }
        let msg = acc.into_message("openai", "gpt-test");
        assert_eq!(msg.stop_reason, StopReason::ToolUse);
        assert_eq!(msg.text(), "Hello");
        assert_eq!(msg.usage.input, 10);
        let calls = msg.tool_calls();
        assert_eq!(calls.len(), 1);
        match calls[0] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call_9");
                assert_eq!(name, "bash");
                assert_eq!(arguments["command"], "ls");
            }
            _ => panic!("expected tool call"),
        }
    }

    #[test]
    fn accumulator_survives_hostile_tool_call_index() {
        // A provider sending index=u64::MAX must not overflow index+1 or OOM.
        let mut acc = Accumulator::default();
        acc.apply_chunk(&json!({"choices":[{"delta":{"tool_calls":[
            {"index": u64::MAX, "id":"x","function":{"name":"t","arguments":"{}"}}
        ]},"finish_reason":"tool_calls"}]}));
        assert!(acc.tool_calls.len() <= MAX_TOOL_CALLS);
        // does not panic:
        let _ = acc.into_message("p", "m");
    }

    #[test]
    fn accumulator_malformed_args_fall_back_to_empty_object() {
        let mut acc = Accumulator::default();
        acc.apply_chunk(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"t","arguments":"{\"a\":"}}]},"finish_reason":"tool_calls"}]}));
        let msg = acc.into_message("p", "m");
        match &msg.content[0] {
            ContentBlock::ToolCall { arguments, .. } => assert_eq!(*arguments, json!({})),
            _ => panic!(),
        }
    }
}
