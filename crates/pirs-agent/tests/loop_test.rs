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
