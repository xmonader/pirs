use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::{Agent, AgentTool, ExecutionMode, ToolExecContext, ToolOutput};
use pirs_ai::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent,
};
use serde_json::{json, Value};

struct MockProvider {
    scripted: Mutex<VecDeque<AssistantMessage>>,
    seen: Arc<Mutex<Vec<Context>>>,
    seen_models: Arc<Mutex<Vec<String>>>,
}

impl MockProvider {
    fn new(messages: Vec<AssistantMessage>) -> Self {
        MockProvider {
            scripted: Mutex::new(messages.into()),
            seen: Arc::new(Mutex::new(Vec::new())),
            seen_models: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn stream(
        &self,
        _model: &str,
        context: &Context,
        _options: &CompletionOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures::stream::BoxStream<'static, StreamEvent> {
        self.seen.lock().unwrap().push(Context {
            system_prompt: context.system_prompt.clone(),
            messages: context.messages.clone(),
            tools: vec![],
        });
        self.seen_models.lock().unwrap().push(_model.to_string());
        let msg = self
            .scripted
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| AssistantMessage {
                content: vec![ContentBlock::text("done")],
                stop_reason: StopReason::Stop,
                ..Default::default()
            });
        let text = msg.text();
        let events = vec![
            StreamEvent::Start,
            StreamEvent::TextDelta(text),
            StreamEvent::Done(Box::new(msg)),
        ];
        Box::pin(futures::stream::iter(events))
    }
}

struct EchoTool;

#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the input text"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text(format!(
            "echo: {}",
            ctx.args["text"].as_str().unwrap_or("")
        )))
    }
}

struct FailingTool;

#[async_trait]
impl AgentTool for FailingTool {
    fn name(&self) -> &str {
        "fail"
    }
    fn description(&self) -> &str {
        "Always fails"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        anyhow::bail!("boom")
    }
}

struct TerminateTool;

#[async_trait]
impl AgentTool for TerminateTool {
    fn name(&self) -> &str {
        "finish"
    }
    fn description(&self) -> &str {
        "Terminate the loop"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text("bye").terminate())
    }
}

/// A tool that pushes a steering message into a shared queue when called — used to
/// prove that a message queued *mid-run* (from inside a tool execution) is injected
/// at the next turn boundary.
struct SteerOnCallTool(pirs_agent::steering::SteeringQueue);

#[async_trait]
impl AgentTool for SteerOnCallTool {
    fn name(&self) -> &str {
        "steer_now"
    }
    fn description(&self) -> &str {
        "Queues a steering message"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        self.0.push("mid-run steer: also check the edge case");
        Ok(ToolOutput::text("queued"))
    }
}

fn tool_call_msg(id: &str, name: &str, args: Value) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
        }],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

fn text_msg(t: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::text(t)],
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

fn make_agent(provider: MockProvider, tools: Vec<Arc<dyn AgentTool>>) -> Agent {
    Agent::new(Arc::new(provider), "mock-model").with_tools(tools)
}

#[tokio::test]
async fn loop_executes_tool_then_answers() {
    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "echo", json!({"text": "hi"})),
        text_msg("final answer"),
    ]);
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)]);
    let new = agent.prompt("go").await.unwrap();

    assert!(new.iter().any(|m| matches!(m, Message::ToolResult(r) if r.content[0].as_text() == Some("echo: hi") && !r.is_error)));
    let last = agent.messages.last().unwrap();
    match last {
        Message::Assistant(a) => assert_eq!(a.text(), "final answer"),
        _ => panic!("expected assistant last"),
    }
}

#[tokio::test]
async fn tool_error_becomes_error_result_and_loop_continues() {
    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "fail", json!({})),
        text_msg("recovered"),
    ]);
    let mut agent = make_agent(provider, vec![Arc::new(FailingTool)]);
    let new = agent.prompt("go").await.unwrap();

    assert!(new.iter().any(
        |m| matches!(m, Message::ToolResult(r) if r.is_error && r.content[0].as_text() == Some("boom"))
    ));
    assert!(
        matches!(agent.messages.last(), Some(Message::Assistant(a)) if a.text() == "recovered")
    );
}

#[tokio::test]
async fn errored_turn_with_tool_call_gets_synthetic_result() {
    // Assistant emits a tool_use but the turn ends in Error (e.g. transport
    // drop mid-stream). Without a matching tool_result the next Anthropic
    // request 400s forever, so the loop must synthesize one.
    let mut errored = tool_call_msg("c1", "echo", json!({"text": "hi"}));
    errored.stop_reason = StopReason::Error;
    let provider = MockProvider::new(vec![errored]);
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)]);
    let new = agent.prompt("go").await.unwrap();

    // History must be valid: the tool_use is followed by a tool_result for c1.
    let assistant_idx = new
        .iter()
        .position(|m| matches!(m, Message::Assistant(a) if !a.tool_calls().is_empty()))
        .expect("assistant with tool call present");
    assert!(
        new[assistant_idx + 1..]
            .iter()
            .any(|m| matches!(m, Message::ToolResult(r) if r.tool_call_id == "c1" && r.is_error)),
        "errored turn must be followed by a synthetic tool_result for c1"
    );
}

#[tokio::test]
async fn invalid_args_rejected_without_executing() {
    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "echo", json!({"wrong": 1})),
        text_msg("ok"),
    ]);
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)]);
    let new = agent.prompt("go").await.unwrap();
    assert!(new.iter().any(
        |m| matches!(m, Message::ToolResult(r) if r.is_error && r.content[0].as_text().unwrap().contains("Invalid arguments"))
    ));
}

#[tokio::test]
async fn unknown_tool_reported() {
    let provider = MockProvider::new(vec![tool_call_msg("c1", "nope", json!({})), text_msg("ok")]);
    let mut agent = make_agent(provider, vec![]);
    let new = agent.prompt("go").await.unwrap();
    assert!(new.iter().any(|m| {
        matches!(
            m,
            Message::ToolResult(r) if r.is_error
                && r.content[0].as_text().is_some_and(|t| t.contains("Tool `nope` not found"))
        )
    }));
}

#[tokio::test]
async fn terminate_stops_loop() {
    let provider = MockProvider::new(vec![tool_call_msg("c1", "finish", json!({}))]);
    let mut agent = make_agent(provider, vec![Arc::new(TerminateTool)]);
    let new = agent.prompt("go").await.unwrap();
    assert!(matches!(new.last(), Some(Message::ToolResult(_))));
}

#[tokio::test]
async fn length_stop_reason_fails_tool_calls_without_executing() {
    let mut msg = tool_call_msg("c1", "echo", json!({"text": "x"}));
    msg.stop_reason = StopReason::Length;
    let provider = MockProvider::new(vec![msg, text_msg("after")]);
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)]);
    let new = agent.prompt("go").await.unwrap();
    assert!(new.iter().any(
        |m| matches!(m, Message::ToolResult(r) if r.is_error && r.content[0].as_text().unwrap().contains("truncated"))
    ));
}

#[tokio::test]
async fn steering_message_injected_mid_run() {
    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "echo", json!({"text": "x"})),
        text_msg("answer"),
    ]);
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)]);
    agent.steer(Message::user("steered"));
    let new = agent.prompt("go").await.unwrap();
    assert!(new.iter().any(
        |m| matches!(m, Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t == "steered"))
    ));
}

#[tokio::test]
async fn steering_queue_primitive_injects_mid_run() {
    use pirs_agent::agent_loop::{run_agent_loop, Budgets, LoopConfig};
    use pirs_agent::events::Hooks;
    use pirs_agent::steering::SteeringQueue;
    use pirs_agent::Emit;

    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![
        // Turn 1: call the tool, which queues a steering message mid-run.
        tool_call_msg("c1", "steer_now", json!({})),
        // Turn 2 (only reached if steering keeps the loop alive): final answer.
        text_msg("answer"),
    ]));
    let queue = SteeringQueue::new();
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(SteerOnCallTool(queue.clone()))];

    let cfg = LoopConfig {
        model: "m".into(),
        completion: CompletionOptions::default(),
        tool_execution: ExecutionMode::Parallel,
        hooks: Hooks {
            get_steering_messages: Some(queue.as_hook()),
            ..Default::default()
        },
        compaction: None,
        visible_tools: None,
        extra_usage: Arc::new(Mutex::new(pirs_ai::Usage::default())),
        cascade: None,
        budgets: Budgets::default(),
        thrash: None,
        skip_remaining_if: None,
    };
    let emit: Emit = Arc::new(|_| {});
    let mut ctx = Context {
        system_prompt: None,
        messages: vec![],
        tools: vec![],
    };
    let (msgs, _) = run_agent_loop(
        vec![Message::user("go")],
        &mut ctx,
        &tools,
        &provider,
        &cfg,
        &emit,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    // The queued message was injected as a user turn...
    let steer_idx = msgs.iter().position(|m| {
        matches!(m, Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("edge case")))
    });
    let steer_idx = steer_idx.expect("steering message must be injected");
    // ...and it landed AFTER the tool result (i.e. mid-run, not with the initial prompt).
    let tool_idx = msgs
        .iter()
        .position(|m| matches!(m, Message::ToolResult(_)))
        .expect("tool result present");
    assert!(
        steer_idx > tool_idx,
        "steering must inject after the tool ran, not before"
    );
    // ...and the loop continued to the final answer because of it.
    assert!(msgs
        .iter()
        .any(|m| matches!(m, Message::Assistant(a) if a.text() == "answer")));
}

#[tokio::test]
async fn parallel_results_in_source_order() {
    let provider = MockProvider::new(vec![
        AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: "a".into(),
                    name: "echo".into(),
                    arguments: json!({"text": "1"}),
                    thought_signature: None,
                },
                ContentBlock::ToolCall {
                    id: "b".into(),
                    name: "echo".into(),
                    arguments: json!({"text": "2"}),
                    thought_signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        },
        text_msg("done"),
    ]);
    let mut agent =
        make_agent(provider, vec![Arc::new(EchoTool)]).with_tool_execution(ExecutionMode::Parallel);
    let new = agent.prompt("go").await.unwrap();
    let results: Vec<&str> = new
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => r.content[0].as_text(),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["echo: 1", "echo: 2"]);
}

/// Records a (path, start, end) span for every call, sleeping briefly mid-call
/// so overlapping calls are actually observable as overlapping spans.
struct SpanTool(Arc<Mutex<Vec<(String, std::time::Instant, std::time::Instant)>>>);

#[async_trait]
impl AgentTool for SpanTool {
    fn name(&self) -> &str {
        "touch"
    }
    fn description(&self) -> &str {
        "Touches a path, recording when it ran"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let path = ctx.args["path"].as_str().unwrap_or("").to_string();
        let start = std::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let end = std::time::Instant::now();
        self.0.lock().unwrap().push((path, start, end));
        Ok(ToolOutput::text("touched"))
    }
}

fn spans_overlap(
    a: &(String, std::time::Instant, std::time::Instant),
    b: &(String, std::time::Instant, std::time::Instant),
) -> bool {
    a.1 < b.2 && b.1 < a.2
}

#[tokio::test]
async fn same_path_calls_serialize_but_different_paths_stay_concurrent() {
    let spans = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider::new(vec![
        AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: "a".into(),
                    name: "touch".into(),
                    arguments: json!({"path": "same.txt"}),
                    thought_signature: None,
                },
                ContentBlock::ToolCall {
                    id: "b".into(),
                    name: "touch".into(),
                    arguments: json!({"path": "same.txt"}),
                    thought_signature: None,
                },
                ContentBlock::ToolCall {
                    id: "c".into(),
                    name: "touch".into(),
                    arguments: json!({"path": "other.txt"}),
                    thought_signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        },
        text_msg("done"),
    ]);
    let mut agent = make_agent(provider, vec![Arc::new(SpanTool(spans.clone()))])
        .with_tool_execution(ExecutionMode::Parallel);
    agent.prompt("go").await.unwrap();

    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 3);
    let same_a = spans.iter().find(|(p, ..)| p == "same.txt").unwrap();
    let same_b = spans
        .iter()
        .filter(|(p, ..)| p == "same.txt")
        .nth(1)
        .unwrap();
    let other = spans.iter().find(|(p, ..)| p == "other.txt").unwrap();

    assert!(
        !spans_overlap(same_a, same_b),
        "two calls on the same path must not run concurrently: {same_a:?} vs {same_b:?}"
    );
    assert!(
        spans_overlap(same_a, other) || spans_overlap(same_b, other),
        "a call on a different path must still run concurrently with the same-path calls"
    );
}

/// Regression test (found via comparison against pi's file-mutation-queue,
/// which keys its lock on a realpath-resolved path for exactly this reason):
/// two different *spellings* of the same file — a plain relative path and a
/// `./`-prefixed one — must still serialize. Without canonicalizing the lock
/// key, these would hash to different map entries and silently run
/// concurrently, defeating the whole point of the same-path lock.
#[tokio::test]
async fn same_file_via_different_path_spellings_still_serializes() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("same.txt");
    std::fs::write(&target, "x").unwrap();
    let plain = target.to_str().unwrap().to_string();
    let dotted = format!("{}/./same.txt", dir.path().to_str().unwrap());

    let spans = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider::new(vec![
        AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: "a".into(),
                    name: "touch".into(),
                    arguments: json!({"path": plain}),
                    thought_signature: None,
                },
                ContentBlock::ToolCall {
                    id: "b".into(),
                    name: "touch".into(),
                    arguments: json!({"path": dotted}),
                    thought_signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        },
        text_msg("done"),
    ]);
    let mut agent = make_agent(provider, vec![Arc::new(SpanTool(spans.clone()))])
        .with_tool_execution(ExecutionMode::Parallel);
    agent.prompt("go").await.unwrap();

    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 2);
    assert!(
        !spans_overlap(&spans[0], &spans[1]),
        "two spellings of the same file must still serialize: {spans:?}"
    );
}

#[tokio::test]
async fn error_stop_reason_ends_run() {
    let provider = MockProvider::new(vec![AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some("api exploded".into()),
        ..Default::default()
    }]);
    let mut agent = make_agent(provider, vec![]);
    let new = agent.prompt("go").await.unwrap();
    let last = new.last().unwrap();
    assert!(matches!(last, Message::Assistant(a) if a.stop_reason == StopReason::Error));
}

#[tokio::test]
async fn auto_compaction_fires_on_threshold() {
    use pirs_agent::compaction::{CompactionConfig, SUMMARY_PREFIX};

    let mut turn1 = tool_call_msg("c1", "echo", json!({"text": "hi"}));
    turn1.usage = pirs_ai::Usage {
        input: 90_000,
        ..Default::default()
    };
    let provider = MockProvider::new(vec![
        turn1,
        text_msg("SUMMARY: user wants a ported loop"),
        text_msg("continuing"),
    ]);
    let seen = Arc::clone(&provider.seen);
    let mut agent =
        make_agent(provider, vec![Arc::new(EchoTool)]).with_compaction(Some(CompactionConfig {
            context_window: 100_000,
            reserve_tokens: 16_000,
            keep_recent_tokens: 10,
            min_recent_user_turns: 0,
        }));
    agent.messages = vec![
        Message::user("old task"),
        Message::Assistant(text_msg("did some earlier work")),
        Message::user("carry on"),
    ];
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 3, "turn1 + summarize + turn2");

    let summary_call = &calls[1];
    assert!(summary_call
        .system_prompt
        .as_deref()
        .unwrap_or("")
        .contains("summarizing a conversation"));

    let turn2_call = &calls[2];
    let first = &turn2_call.messages[0];
    assert!(
        matches!(first, Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains(SUMMARY_PREFIX) && t.contains("SUMMARY: user wants a ported loop")))
    );

    assert!(agent.messages.iter().any(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains(SUMMARY_PREFIX))
    )));
}

#[tokio::test]
async fn no_compaction_below_threshold() {
    let provider = MockProvider::new(vec![text_msg("fine")]);
    let seen = Arc::clone(&provider.seen);
    let mut agent = make_agent(provider, vec![])
        .with_compaction(Some(pirs_agent::compaction::CompactionConfig::default()));
    agent.prompt("go").await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tool_diet_blocks_hidden_tool_until_loaded() {
    use pirs_agent::use_tool::UseTool;
    use std::collections::HashSet;

    let visible: pirs_agent::agent_loop::VisibleTools =
        Arc::new(Mutex::new(HashSet::from(["use_tool".to_string()])));
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool)];
    let use_tool = UseTool::new(&visible, &tools);
    let mut all_tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool), use_tool];

    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "echo", json!({"text": "blocked"})),
        AssistantMessage {
            content: vec![
                ContentBlock::ToolCall {
                    id: "c2".into(),
                    name: "use_tool".into(),
                    arguments: json!({"name": "echo"}),
                    thought_signature: None,
                },
                ContentBlock::ToolCall {
                    id: "c3".into(),
                    name: "echo".into(),
                    arguments: json!({"text": "now works"}),
                    thought_signature: None,
                },
            ],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        },
        text_msg("done"),
    ]);
    all_tools.truncate(2);
    let mut agent = make_agent(provider, all_tools).with_visible_tools(Some(visible));
    let new = agent.prompt("go").await.unwrap();

    let blocked = new.iter().any(|m| matches!(
        m,
        Message::ToolResult(r) if r.is_error && r.content[0].as_text().unwrap().contains("not loaded in this session")
    ));
    assert!(
        blocked,
        "hidden tool call should be rejected with use_tool hint"
    );

    let loaded = new.iter().any(|m| matches!(
        m,
        Message::ToolResult(r) if !r.is_error && r.tool_name == "use_tool" && r.content[0].as_text().unwrap().contains("Tool 'echo' loaded")
    ));
    assert!(loaded);

    let worked = new.iter().any(|m| matches!(
        m,
        Message::ToolResult(r) if !r.is_error && r.tool_name == "echo" && r.content[0].as_text() == Some("echo: now works")
    ));
    assert!(worked, "tool should work after use_tool loads it");
}

#[tokio::test]
async fn delegate_model_override_routes_subagent() {
    use pirs_agent::delegate::DelegateTool;

    let provider = Arc::new(MockProvider::new(vec![
        tool_call_msg(
            "c1",
            "delegate",
            json!({"task": "easy step", "model": "weak-model"}),
        ),
        text_msg("weak answer"),
        text_msg("planner done"),
    ]));
    let seen_models = Arc::clone(&provider.seen_models);

    let delegate = DelegateTool::new(
        provider.clone(),
        "strong-model",
        CompletionOptions::default(),
        || vec![Arc::new(EchoTool)],
    );
    let mut agent = Agent::new(provider, "strong-model").with_tools(vec![delegate]);
    agent.prompt("plan").await.unwrap();

    let models = seen_models.lock().unwrap().clone();
    assert_eq!(models, vec!["strong-model", "weak-model", "strong-model"]);
}

#[tokio::test]
async fn delegate_runs_subagent_with_fresh_context() {
    use pirs_agent::delegate::DelegateTool;

    let shared_seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(MockProvider {
        scripted: Mutex::new(
            vec![
                tool_call_msg("c1", "delegate", json!({"task": "count words in 'a b c'"})),
                text_msg("3 words"),
                text_msg("final: 3 words"),
            ]
            .into(),
        ),
        seen: Arc::clone(&shared_seen),
        seen_models: Arc::new(Mutex::new(Vec::new())),
    });

    let delegate = DelegateTool::new(
        provider.clone(),
        "mock-model",
        CompletionOptions::default(),
        || vec![Arc::new(EchoTool)],
    );
    let mut agent = Agent::new(provider, "mock-model").with_tools(vec![delegate]);
    let new = agent.prompt("go").await.unwrap();

    let delegated = new.iter().any(|m| matches!(
        m,
        Message::ToolResult(r) if !r.is_error && r.tool_name == "delegate" && r.content[0].as_text() == Some("3 words")
    ));
    assert!(
        delegated,
        "delegate result should be the sub-agent's answer"
    );

    let calls = shared_seen.lock().unwrap();
    assert_eq!(calls.len(), 3, "main turn1 + sub-agent turn + main turn2");
    let sub_ctx = &calls[1];
    assert!(
        !sub_ctx.messages.iter().any(|m| matches!(m, Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t == "go"))),
        "sub-agent must not see the main conversation"
    );
}

#[tokio::test]
async fn delegate_inherits_policy_hooks() {
    use pirs_agent::delegate::DelegateTool;

    let provider = Arc::new(MockProvider::new(vec![
        tool_call_msg("c1", "delegate", json!({"task": "run echo"})),
        tool_call_msg("sub", "echo", json!({"text": "x"})),
        text_msg("sub finished"),
        text_msg("main done"),
    ]));

    let blocker: pirs_agent::events::BeforeToolCallHook =
        Arc::new(|_id, name, _args| Some(format!("policy denies {name}")));
    let passthrough: pirs_agent::events::AfterToolCallHook = Arc::new(|_, _, _| None);

    let delegate = DelegateTool::new(
        provider.clone(),
        "mock-model",
        CompletionOptions::default(),
        || vec![Arc::new(EchoTool)],
    );
    delegate.with_policy_hooks(blocker, passthrough);

    let mut agent = Agent::new(provider.clone(), "mock-model").with_tools(vec![delegate]);
    agent.prompt("go").await.unwrap();

    let calls = provider.seen.lock().unwrap();
    let sub_turn2 = calls
        .iter()
        .skip(1)
        .find(|ctx| {
            ctx.messages.iter().any(|m| matches!(
                m,
                Message::ToolResult(r) if r.is_error && r.content[0].as_text().map(|t| t.contains("policy denies echo")).unwrap_or(false)
            ))
        });
    assert!(
        sub_turn2.is_some(),
        "policy hook must apply inside sub-agent: {calls:?}"
    );
}

#[tokio::test]
async fn delegate_background_job_completes_and_is_steerable() {
    use pirs_agent::delegate::DelegateTool;
    use pirs_agent::jobs::{self, JobStatus};

    let provider = Arc::new(MockProvider::new(vec![
        tool_call_msg(
            "c1",
            "delegate",
            json!({"task": "short job", "background": true}),
        ),
        text_msg("main done"),
    ]));
    let sub_provider = Arc::new(MockProvider::new(vec![text_msg("bg answer")]));
    let delegate = DelegateTool::new(
        sub_provider,
        "mock-model",
        CompletionOptions::default(),
        || vec![Arc::new(EchoTool)],
    );
    let mut agent = Agent::new(provider, "mock-model").with_tools(vec![delegate]);
    let new = agent.prompt("go").await.unwrap();

    let started = new.iter().any(|m| {
        matches!(
            m,
            Message::ToolResult(r) if r.content[0].as_text().unwrap().contains("background agent #")
        )
    });
    assert!(started, "{new:?}");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let registry = jobs::registry();
    let list = registry.list();
    let finished = list
        .iter()
        .find(|l| l.contains("agent") && l.contains("exited(0)"));
    assert!(finished.is_some(), "{list:?}");
    let id: u64 = finished
        .unwrap()
        .trim_start_matches('#')
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let job = registry.get(id).unwrap();
    let path = job.lock().unwrap().output_path.clone();
    let answer = std::fs::read_to_string(path).unwrap();
    assert_eq!(answer, "bg answer");
    assert_eq!(job.lock().unwrap().status, JobStatus::Exited(0));
}

#[tokio::test]
async fn bash_background_job_runs_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let tool = pirs_tools::BashTool::new(dir.path().to_path_buf());
    let out = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"command": "sleep 0.2; echo done > bg.txt", "background": true}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert!(out.content[0]
        .as_text()
        .unwrap()
        .contains("background job #"));
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(dir.path().join("bg.txt").exists());
    let list = pirs_agent::jobs::registry().list();
    assert!(list.iter().any(|l| l.contains("exited(0)")), "{list:?}");
}

#[tokio::test]
async fn cascade_escalates_on_judge_reject_and_keeps_good_draft() {
    use pirs_agent::agent_loop::{CascadeConfig, CascadeJudge};

    let provider = Arc::new(MockProvider::new(vec![
        AssistantMessage::default(), // bad draft (empty)
        text_msg("main model answer"),
    ]));
    let seen_models = Arc::clone(&provider.seen_models);
    let judge: CascadeJudge = Arc::new(|draft| {
        let ok = !draft.text().trim().is_empty();
        Box::pin(async move { ok })
    });
    let mut agent = Agent::new(provider, "mock-model").with_cascade(Some(CascadeConfig {
        draft_model: "draft-model".into(),
        judge,
    }));
    agent.prompt("go").await.unwrap();

    let models = seen_models.lock().unwrap().clone();
    assert_eq!(
        models,
        vec!["draft-model", "mock-model"],
        "draft then escalate"
    );
    assert!(matches!(
        agent.messages.last(),
        Some(Message::Assistant(a)) if a.text() == "main model answer"
    ));

    let provider2 = Arc::new(MockProvider::new(vec![text_msg("draft is fine")]));
    let seen_models2 = Arc::clone(&provider2.seen_models);
    let mut agent2 = Agent::new(provider2, "mock-model").with_cascade(Some(CascadeConfig {
        draft_model: "draft-model".into(),
        judge: Arc::new(|draft| {
            let ok = !draft.text().trim().is_empty();
            Box::pin(async move { ok })
        }),
    }));
    agent2.prompt("go").await.unwrap();
    assert_eq!(
        seen_models2.lock().unwrap().clone(),
        vec!["draft-model"],
        "accepted draft: no escalation"
    );
}
#[tokio::test]
async fn assistant_message_appears_exactly_once_per_turn() {
    let provider = MockProvider::new(vec![text_msg("answer")]);
    let mut agent = make_agent(provider, vec![]);
    agent.prompt("go").await.unwrap();
    let assistants = agent
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Assistant(_)))
        .count();
    assert_eq!(
        assistants,
        1,
        "assistant message duplicated in context: {:?}",
        agent
            .messages
            .iter()
            .map(|m| match m {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
            })
            .collect::<Vec<_>>()
    );
}

/// Parent cancel must propagate into a running delegate sub-agent. Regression:
/// the watcher captured a token that begin_prompt re-minted, so it cancelled a
/// dead token and the sub-agent ran to completion anyway.
#[tokio::test]
async fn parent_cancel_aborts_delegate_subagent() {
    use pirs_agent::delegate::DelegateTool;

    // Main model asks for a delegate; the sub-agent's model blocks until
    // cancelled (never completes on its own).
    let main_provider = Arc::new(MockProvider::new(vec![tool_call_msg(
        "c1",
        "delegate",
        json!({"task": "block forever"}),
    )]));

    struct BlockingSubProvider;
    #[async_trait]
    impl LlmProvider for BlockingSubProvider {
        async fn stream(
            &self,
            model: &str,
            _context: &Context,
            _options: &CompletionOptions,
            cancel: tokio_util::sync::CancellationToken,
        ) -> futures::stream::BoxStream<'static, StreamEvent> {
            let model = model.to_string();
            Box::pin(futures::stream::once(async move {
                cancel.cancelled().await;
                StreamEvent::Done(Box::new(AssistantMessage {
                    provider: "mock".into(),
                    api: "mock".into(),
                    model,
                    stop_reason: StopReason::Aborted,
                    ..Default::default()
                }))
            }))
        }
    }

    let delegate = DelegateTool::new(
        Arc::new(BlockingSubProvider),
        "mock-model",
        CompletionOptions::default(),
        || vec![Arc::new(EchoTool)],
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ToolExecContext {
        tool_call_id: "c1".into(),
        args: json!({"task": "block forever"}),
        cancel: cancel.clone(),
        on_update: None,
    };
    let _ = main_provider; // documents intent; the tool is exercised directly
    let handle = tokio::spawn(async move { delegate.execute(ctx).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();

    let out = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("delegate did not return after parent cancel — sub-agent still running")
        .unwrap();
    // The sub-agent aborted, so the delegate surfaces an error, not a hang.
    assert!(
        out.is_err()
            || out.unwrap().content[0]
                .as_text()
                .map(|t| !t.is_empty())
                .unwrap_or(false)
    );
}

/// A second tool answering to the same name as `EchoTool`, standing in for a
/// rhai pack overriding a native tool (e.g. wrapping `bash` in a sandbox).
struct OverrideEchoTool;

#[async_trait]
impl AgentTool for OverrideEchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "overridden echo"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text(format!(
            "overridden: {}",
            ctx.args["text"].as_str().unwrap_or("")
        )))
    }
}

/// Regression test: a later-registered tool sharing a name with an earlier
/// one (the shape of a rhai pack overriding a native tool by re-registering
/// its name) must win both in dispatch and in the schema sent to the model —
/// never leave two ambiguous "echo" entries or silently dispatch to the
/// shadowed original.
#[tokio::test]
async fn later_registered_tool_overrides_same_named_earlier_one() {
    let defs = pirs_agent::tool_defs(&[
        Arc::new(EchoTool) as Arc<dyn AgentTool>,
        Arc::new(OverrideEchoTool) as Arc<dyn AgentTool>,
    ]);
    let echoes: Vec<_> = defs.iter().filter(|d| d.name == "echo").collect();
    assert_eq!(echoes.len(), 1, "must not list two same-named tools");
    assert_eq!(echoes[0].description, "overridden echo");

    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "echo", json!({"text": "hi"})),
        text_msg("done"),
    ]);
    let mut agent = make_agent(
        provider,
        vec![Arc::new(EchoTool), Arc::new(OverrideEchoTool)],
    );
    let new = agent.prompt("go").await.unwrap();
    let result_text = new
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => r.content[0].as_text(),
            _ => None,
        })
        .unwrap();
    assert_eq!(result_text, "overridden: hi");
}

/// Host-enforced pin coexistence: even if transform_context strips every
/// system-reminder, protected kinds (stop_gate) are restored before the LLM sees
/// the context.
#[tokio::test]
async fn host_restores_stop_gate_when_transform_strips_all_reminders() {
    use pirs_agent::{wrap_reminder, Hooks};

    let scripted = vec![text_msg("ok")];
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MockProvider {
        scripted: Mutex::new(scripted.into()),
        seen: Arc::clone(&seen),
        seen_models: Arc::new(Mutex::new(Vec::new())),
    };

    // Malicious/naive pack: drop every system-reminder user message.
    let strip_all = Arc::new(|msgs: Vec<Message>| {
        msgs.into_iter()
            .filter(|m| match m {
                Message::User(u) => match &u.content {
                    pirs_ai::UserContent::Text(t) => !t.contains("<system-reminder>"),
                    _ => true,
                },
                _ => true,
            })
            .collect()
    }) as pirs_agent::events::TransformContextHook;

    let mut agent = Agent::new(Arc::new(provider), "mock-model").with_hooks(Hooks {
        transform_context: Some(strip_all),
        ..Default::default()
    });

    // History already has a stop_gate control pin (as on_follow_up would inject).
    agent.messages.push(Message::user(wrap_reminder(
        "stop_gate",
        "STOP GATE: run tests before finishing",
    )));
    agent.messages.push(Message::user(wrap_reminder(
        "plan",
        "1. edit\n2. verify",
    )));

    agent.prompt("I am done").await.unwrap();

    let calls = seen.lock().unwrap();
    assert!(!calls.is_empty(), "provider must have been called");
    let first = &calls[0].messages;
    let has_gate = first.iter().any(|m| match m {
        Message::User(u) => match &u.content {
            pirs_ai::UserContent::Text(t) => {
                t.contains("kind=stop_gate") && t.contains("STOP GATE")
            }
            _ => false,
        },
        _ => false,
    });
    assert!(
        has_gate,
        "LLM-facing context must still carry stop_gate after hostile transform: {first:?}"
    );
}

/// Thrash stop must still leave every tool_use paired with a tool_result.
/// Regression: early-return used to drop batch results and wedge the session.
#[tokio::test]
async fn thrash_stop_keeps_tool_use_result_pairs() {
    use pirs_agent::agent_loop::{run_agent_loop, Budgets, LoopConfig};
    use pirs_agent::compaction::tool_pairs_intact;
    use pirs_agent::events::Hooks;
    use pirs_agent::{Emit, ThrashGuard};

    // One assistant turn with three identical tool calls — thrash trips on 2nd.
    let multi = AssistantMessage {
        content: vec![
            ContentBlock::ToolCall {
                id: "t0".into(),
                name: "echo".into(),
                arguments: json!({"text": "same"}),
                thought_signature: None,
            },
            ContentBlock::ToolCall {
                id: "t1".into(),
                name: "echo".into(),
                arguments: json!({"text": "same"}),
                thought_signature: None,
            },
            ContentBlock::ToolCall {
                id: "t2".into(),
                name: "echo".into(),
                arguments: json!({"text": "same"}),
                thought_signature: None,
            },
        ],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    };

    let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider::new(vec![multi]));
    let thrash = ThrashGuard::with_limits(2, 10);
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool)];
    let cfg = LoopConfig {
        model: "m".into(),
        completion: CompletionOptions::default(),
        tool_execution: ExecutionMode::Sequential,
        hooks: Hooks::default(),
        compaction: None,
        visible_tools: None,
        extra_usage: Arc::new(Mutex::new(pirs_ai::Usage::default())),
        cascade: None,
        budgets: Budgets::default(),
        thrash: Some(thrash),
        skip_remaining_if: None,
    };
    let emit: Emit = Arc::new(|_| {});
    let mut ctx = Context {
        system_prompt: None,
        messages: vec![],
        tools: vec![],
    };
    let (msgs, _) = run_agent_loop(
        vec![Message::user("go")],
        &mut ctx,
        &tools,
        &provider,
        &cfg,
        &emit,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;

    assert!(
        tool_pairs_intact(&msgs),
        "thrash stop must not leave dangling tool_use: {msgs:?}"
    );
    assert!(
        tool_pairs_intact(&ctx.messages),
        "persisted context must keep tool pairs: {:?}",
        ctx.messages
    );
    // Every tool call id has a matching result.
    for id in ["t0", "t1", "t2"] {
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Message::ToolResult(r) if r.tool_call_id == id)),
            "missing tool_result for {id} in {msgs:?}"
        );
    }
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("thrash stop") || t.contains("loop detection"))
        )),
        "expected thrash stop user message: {msgs:?}"
    );
}
