//! VCR replay: sessions are cassettes. `pirs replay <session.jsonl>` re-runs
//! the conversation with recorded assistant messages and recorded tool
//! results — deterministic regression tests for agent behavior. `--model X`
//! swaps in a live model and reports where the trajectory diverges.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use async_trait::async_trait;
use pirs_agent::{Agent, AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent, ToolResultMessage,
};
use serde_json::Value;

pub fn load_cassette(path: &Path) -> anyhow::Result<Vec<Message>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(line).with_context(|| {
                format!("{} line {}: invalid message JSON", path.display(), i + 1)
            })?,
        );
    }
    Ok(out)
}

/// Serves recorded assistant messages in order.
pub struct ReplayProvider {
    responses: Mutex<VecDeque<AssistantMessage>>,
}

impl ReplayProvider {
    pub fn new(cassette: &[Message]) -> Self {
        let responses = cassette
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.clone()),
                _ => None,
            })
            .collect();
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl LlmProvider for ReplayProvider {
    async fn stream(
        &self,
        model: &str,
        _context: &Context,
        _options: &CompletionOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures::stream::BoxStream<'static, StreamEvent> {
        let next = self.responses.lock().unwrap().pop_front();
        let msg = match next {
            Some(m) => m,
            None => AssistantMessage {
                provider: "replay".into(),
                api: "replay".into(),
                model: model.to_string(),
                stop_reason: StopReason::Error,
                error_message: Some(
                    "cassette exhausted: agent asked for more turns than recorded".into(),
                ),
                ..Default::default()
            },
        };
        let text = msg.text();
        let mut events = vec![StreamEvent::Start];
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta(text));
        }
        events.push(StreamEvent::Done(Box::new(msg)));
        Box::pin(futures::stream::iter(events))
    }
}

/// One recorded tool execution: call identity + result.
#[derive(Debug, Clone)]
struct RecordedCall {
    id: String,
    name: String,
    args: Value,
    result: ToolResultMessage,
}

/// Index recorded calls by pairing assistant tool_use blocks with the
/// tool_results that answer them.
fn index_calls(cassette: &[Message]) -> Vec<RecordedCall> {
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    let mut out = Vec::new();
    for m in cassette {
        match m {
            Message::Assistant(a) => {
                for b in &a.content {
                    if let ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } = b
                    {
                        calls.insert(id.clone(), (name.clone(), arguments.clone()));
                    }
                }
            }
            Message::ToolResult(r) => {
                let (name, args) = calls
                    .get(&r.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| (r.tool_name.clone(), Value::Null));
                out.push(RecordedCall {
                    id: r.tool_call_id.clone(),
                    name,
                    args,
                    result: r.clone(),
                });
            }
            _ => {}
        }
    }
    out
}

/// Serves recorded tool results. Strict mode matches by tool_call id (exact,
/// since the replayed assistant messages carry the recorded ids). Live mode
/// (--model) matches by name + args. Anything else is a divergence.
pub struct CassetteTool {
    inner: Arc<dyn AgentTool>,
    by_id: Arc<Mutex<HashMap<String, RecordedCall>>>,
    by_signature: Arc<Mutex<HashMap<String, VecDeque<RecordedCall>>>>,
    live: bool,
    diverged: Arc<Mutex<Option<String>>>,
}

impl CassetteTool {
    pub fn wrap(
        inner: Arc<dyn AgentTool>,
        cassette: &[Message],
        live: bool,
        diverged: Arc<Mutex<Option<String>>>,
    ) -> Self {
        let calls = index_calls(cassette);
        let by_id: HashMap<String, RecordedCall> =
            calls.iter().map(|c| (c.id.clone(), c.clone())).collect();
        let mut by_signature: HashMap<String, VecDeque<RecordedCall>> = HashMap::new();
        for c in calls {
            by_signature
                .entry(format!("{}:{}", c.name, c.args))
                .or_default()
                .push_back(c);
        }
        Self {
            inner,
            by_id: Arc::new(Mutex::new(by_id)),
            by_signature: Arc::new(Mutex::new(by_signature)),
            live,
            diverged,
        }
    }

    fn mark_divergence(&self, what: String) {
        let mut d = self.diverged.lock().unwrap();
        if d.is_none() {
            *d = Some(what);
        }
    }
}

#[async_trait]
impl AgentTool for CassetteTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let recorded = if self.live {
            self.by_signature
                .lock()
                .unwrap()
                .get_mut(&format!("{}:{}", self.inner.name(), ctx.args))
                .and_then(|q| q.pop_front())
        } else {
            self.by_id.lock().unwrap().remove(&ctx.tool_call_id)
        };
        match recorded {
            Some(call) => Ok(ToolOutput {
                content: call.result.content.clone(),
                details: call.result.details.clone(),
                terminate: false,
            }),
            None => {
                let what = format!("tool {}({}) not in cassette", self.inner.name(), ctx.args);
                self.mark_divergence(what.clone());
                if self.live {
                    // Live mode: execute for real so the run can continue.
                    self.inner.execute(ctx).await
                } else {
                    Ok(ToolOutput::text(format!("replay divergence: {what}")))
                }
            }
        }
    }
}

pub enum DivergenceKind {
    MessageCount,
    Text,
    ToolCall,
    ToolResult,
}

pub struct Divergence {
    pub index: usize,
    /// What differed (kept for callers that branch on it; the CLI prints
    /// expected/actual only).
    #[allow(dead_code)]
    pub kind: DivergenceKind,
    pub expected: String,
    pub actual: String,
}

pub struct ReplayReport {
    pub matched: usize,
    pub divergence: Option<Divergence>,
}

fn summarize(m: &Message) -> String {
    match m {
        Message::User(_) => "user".into(),
        Message::Assistant(a) => {
            let calls: Vec<String> = a.tool_calls().iter().map(|c| format!("{c:?}")).collect();
            format!("assistant(text={:?}, calls={calls:?})", a.text())
        }
        Message::ToolResult(r) => {
            let text: String = r
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            format!("tool_result({}={:?})", r.tool_name, text)
        }
    }
}

fn message_eq(a: &Message, b: &Message) -> Option<DivergenceKind> {
    match (a, b) {
        (Message::User(_), Message::User(_)) => None,
        (Message::Assistant(x), Message::Assistant(y)) => {
            let calls_eq = x.tool_calls().len() == y.tool_calls().len()
                && x.tool_calls().iter().zip(y.tool_calls()).all(|(c, d)| {
                    matches!(
                        (c, d),
                        (
                            ContentBlock::ToolCall { name: n1, arguments: a1, .. },
                            ContentBlock::ToolCall { name: n2, arguments: a2, .. }
                        ) if n1 == n2 && a1 == a2
                    )
                });
            if !calls_eq {
                Some(DivergenceKind::ToolCall)
            } else if x.text() != y.text() {
                Some(DivergenceKind::Text)
            } else {
                None
            }
        }
        (Message::ToolResult(x), Message::ToolResult(y)) => {
            let tx: String = x.content.iter().filter_map(|b| b.as_text()).collect();
            let ty: String = y.content.iter().filter_map(|b| b.as_text()).collect();
            if tx == ty && x.is_error == y.is_error {
                None
            } else {
                Some(DivergenceKind::ToolResult)
            }
        }
        _ => Some(DivergenceKind::MessageCount),
    }
}

/// Compare the cassette's non-prompt messages against what the replay
/// produced, reporting the first divergence.
pub fn compare(expected: &[Message], actual: &[Message]) -> ReplayReport {
    let mut matched = 0;
    for (i, e) in expected.iter().enumerate() {
        match actual.get(i) {
            None => {
                return ReplayReport {
                    matched,
                    divergence: Some(Divergence {
                        index: i,
                        kind: DivergenceKind::MessageCount,
                        expected: summarize(e),
                        actual: "<missing>".into(),
                    }),
                }
            }
            Some(a) => {
                if let Some(kind) = message_eq(e, a) {
                    return ReplayReport {
                        matched,
                        divergence: Some(Divergence {
                            index: i,
                            kind,
                            expected: summarize(e),
                            actual: summarize(a),
                        }),
                    };
                }
                matched += 1;
            }
        }
    }
    if actual.len() > expected.len() {
        return ReplayReport {
            matched,
            divergence: Some(Divergence {
                index: expected.len(),
                kind: DivergenceKind::MessageCount,
                expected: "<end>".into(),
                actual: summarize(&actual[expected.len()]),
            }),
        };
    }
    ReplayReport {
        matched,
        divergence: None,
    }
}

/// User prompts in cassette order (tool results are a separate variant).
pub fn prompts_of(cassette: &[Message]) -> Vec<String> {
    cassette
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => match &u.content {
                pirs_ai::UserContent::Text(t) => Some(t.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The full trajectory replay should reproduce (prompts included: the loop
/// records each prompt into new_messages before the turn's output).
pub fn expected_of(cassette: &[Message]) -> Vec<Message> {
    cassette.to_vec()
}

/// Run a cassette through an agent. Returns the produced messages.
pub async fn run_replay(agent: &mut Agent, cassette: &[Message]) -> Vec<Message> {
    let mut produced = Vec::new();
    for prompt in prompts_of(cassette) {
        match agent.prompt(&prompt).await {
            Ok(new) => produced.extend(new),
            Err(_) => break,
        }
    }
    produced
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::Usage;

    fn assistant_text(t: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::text(t)],
            stop_reason: StopReason::Stop,
            usage: Usage::default(),
            ..Default::default()
        })
    }

    fn tool_call(id: &str, name: &str, args: Value) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::text("checking"),
                ContentBlock::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: args,
                    thought_signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        })
    }

    fn tool_result(id: &str, name: &str, text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![ContentBlock::text(text)],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        })
    }

    struct EchoTool;
    #[async_trait]
    impl AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}}})
        }
        async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::text(format!(
                "LIVE:{}",
                ctx.args["text"].as_str().unwrap_or("")
            )))
        }
    }

    fn cassette() -> Vec<Message> {
        vec![
            Message::user("hi"),
            tool_call("c1", "echo", serde_json::json!({"text":"a"})),
            tool_result("c1", "echo", "a"),
            assistant_text("done"),
        ]
    }

    #[test]
    fn compare_identical() {
        let expected = expected_of(&cassette());
        let report = compare(&expected, &expected);
        assert!(report.divergence.is_none());
        assert_eq!(report.matched, 4);
    }

    #[test]
    fn compare_finds_first_divergence() {
        let expected = expected_of(&cassette());
        let mut actual = expected.clone();
        actual[2] = tool_result("c1", "echo", "TAMPERED");
        let report = compare(&expected, &actual);
        let d = report.divergence.unwrap();
        assert_eq!(d.index, 2);
        assert!(matches!(d.kind, DivergenceKind::ToolResult));
    }

    #[tokio::test]
    async fn strict_replay_reproduces_cassette() {
        let tape = cassette();
        let provider = Arc::new(ReplayProvider::new(&tape));
        let diverged = Arc::new(Mutex::new(None));
        let tool = CassetteTool::wrap(Arc::new(EchoTool), &tape, false, Arc::clone(&diverged));
        let mut agent = Agent::new(provider, "replay-model").with_tools(vec![Arc::new(tool)]);

        let produced = run_replay(&mut agent, &tape).await;
        let report = compare(&expected_of(&tape), &produced);
        assert!(
            report.divergence.is_none(),
            "diverged: {:?}",
            report.divergence.map(|d| (d.index, d.expected, d.actual))
        );
        assert!(diverged.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn live_mode_matches_by_signature_and_executes_on_miss() {
        let tape = cassette();
        let diverged = Arc::new(Mutex::new(None));
        let tool = CassetteTool::wrap(Arc::new(EchoTool), &tape, true, Arc::clone(&diverged));

        // Same name+args as recorded (different call id): cassette result.
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "live-1".into(),
                args: serde_json::json!({"text": "a"}),
                cancel: tokio_util::sync::CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert_eq!(out.content[0].as_text().unwrap(), "a");
        assert!(diverged.lock().unwrap().is_none());

        // Unknown args: executes live, records divergence.
        let out2 = tool
            .execute(ToolExecContext {
                tool_call_id: "live-2".into(),
                args: serde_json::json!({"text": "zzz"}),
                cancel: tokio_util::sync::CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert_eq!(out2.content[0].as_text().unwrap(), "LIVE:zzz");
        assert!(diverged.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn strict_replay_detects_tampered_result() {
        let mut tape = cassette();
        tape[2] = tool_result("c1", "echo", "TAMPERED");
        let provider = Arc::new(ReplayProvider::new(&tape));
        let diverged = Arc::new(Mutex::new(None));
        // Tool serves the ORIGINAL echo output (not the cassette's).
        let tool = CassetteTool::wrap(
            Arc::new(EchoTool),
            &cassette(),
            false,
            Arc::clone(&diverged),
        );
        let mut agent = Agent::new(provider, "replay-model").with_tools(vec![Arc::new(tool)]);

        let produced = run_replay(&mut agent, &tape).await;
        let report = compare(&expected_of(&tape), &produced);
        let d = report.divergence.expect("must detect tampering");
        assert_eq!(d.index, 2);
    }
}
