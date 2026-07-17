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
}

impl MockProvider {
    fn new(messages: Vec<AssistantMessage>) -> Self {
        MockProvider {
            scripted: Mutex::new(messages.into()),
            seen: Arc::new(Mutex::new(Vec::new())),
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
    assert!(matches!(agent.messages.last(), Some(Message::Assistant(a)) if a.text() == "recovered"));
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
    let provider = MockProvider::new(vec![
        tool_call_msg("c1", "nope", json!({})),
        text_msg("ok"),
    ]);
    let mut agent = make_agent(provider, vec![]);
    let new = agent.prompt("go").await.unwrap();
    assert!(new.iter().any(
        |m| matches!(m, Message::ToolResult(r) if r.is_error && r.content[0].as_text() == Some("Tool nope not found"))
    ));
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
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)])
        .with_tool_execution(ExecutionMode::Parallel);
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
    let mut agent = make_agent(provider, vec![Arc::new(EchoTool)]).with_compaction(Some(
        CompactionConfig {
            context_window: 100_000,
            reserve_tokens: 16_000,
            keep_recent_tokens: 10,
        },
    ));
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
    assert!(matches!(first, Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains(SUMMARY_PREFIX) && t.contains("SUMMARY: user wants a ported loop"))));

    assert!(agent.messages.iter().any(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains(SUMMARY_PREFIX))
    )));
}

#[tokio::test]
async fn no_compaction_below_threshold() {
    let provider = MockProvider::new(vec![text_msg("fine")]);
    let seen = Arc::clone(&provider.seen);
    let mut agent = make_agent(provider, vec![]).with_compaction(Some(
        pirs_agent::compaction::CompactionConfig::default(),
    ));
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
    assert!(blocked, "hidden tool call should be rejected with use_tool hint");

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
    assert!(delegated, "delegate result should be the sub-agent's answer");

    let calls = shared_seen.lock().unwrap();
    assert_eq!(calls.len(), 3, "main turn1 + sub-agent turn + main turn2");
    let sub_ctx = &calls[1];
    assert!(
        !sub_ctx.messages.iter().any(|m| matches!(m, Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t == "go"))),
        "sub-agent must not see the main conversation"
    );
}
