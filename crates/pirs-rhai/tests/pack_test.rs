use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::{Agent, AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent,
};
use pirs_rhai::ExtensionHost;
use serde_json::{json, Value};

const PACK: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../extensions/weak-model.rhai"
);

struct MockProvider {
    scripted: Mutex<VecDeque<AssistantMessage>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
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
        self.seen.lock().unwrap().push(context.messages.clone());
        let msg = self
            .scripted
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| AssistantMessage {
                content: vec![ContentBlock::text("fallback")],
                stop_reason: StopReason::Stop,
                ..Default::default()
            });
        Box::pin(futures::stream::iter(vec![StreamEvent::Done(Box::new(
            msg,
        ))]))
    }
}

struct NamedTool {
    name: String,
}

#[async_trait]
impl AgentTool for NamedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "dummy"
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}}})
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text(format!("{} done", self.name)))
    }
}

fn tc(id: &str, name: &str, args: Value) -> AssistantMessage {
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

fn text(t: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::text(t)],
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

fn build(
    script_msgs: Vec<AssistantMessage>,
    tools: Vec<Arc<dyn AgentTool>>,
) -> (Agent, Arc<Mutex<Vec<Vec<Message>>>>) {
    let mut host = ExtensionHost::new();
    host.load_source(&std::fs::read_to_string(PACK).unwrap(), PACK.into())
        .unwrap();
    let host = Arc::new(host);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(MockProvider {
        scripted: Mutex::new(script_msgs.into()),
        seen: Arc::clone(&seen),
    });
    let mut tools = tools;
    tools.extend(host.tools());
    let agent = Agent::new(provider, "mock")
        .with_tools(tools)
        .with_hooks(host.hooks());
    (agent, seen)
}

#[tokio::test]
async fn loop_detector_blocks_third_identical_call() {
    let (mut agent, _seen) = build(
        vec![
            tc("1", "bash", json!({"path": "/x"})),
            tc("2", "bash", json!({"path": "/x"})),
            tc("3", "bash", json!({"path": "/x"})),
            text("gave up"),
        ],
        vec![Arc::new(NamedTool {
            name: "bash".into(),
        })],
    );
    let new = agent.prompt("go").await.unwrap();

    let blocked = new.iter().any(|m| matches!(
        m,
        Message::ToolResult(r) if r.is_error && r.content.iter().any(|b| b.as_text().is_some_and(|t|
            t.contains("fingerprint was already used")
                || t.contains("loop detection")
                || t.contains("identical args")
        ))
    ));
    assert!(blocked, "third identical call should be blocked: {new:?}");
}

#[tokio::test]
async fn verify_after_edit_steers_model_to_test() {
    let (mut agent, seen) = build(
        vec![tc("1", "edit", json!({"path": "f.rs"})), text("done")],
        vec![Arc::new(NamedTool {
            name: "edit".into(),
        })],
    );
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    assert!(calls.len() >= 2, "steering should trigger a second turn");
    let second = &calls[1];
    assert!(second.iter().any(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("kind=verify") && t.contains("build and tests"))
    )));
}

#[tokio::test]
async fn stop_gate_forces_verify_after_edit_before_finish() {
    // edit succeeds → model says "done" with no bash → follow-up should fire.
    let (mut agent, seen) = build(
        vec![
            tc("1", "edit", json!({"path": "f.rs"})),
            text("all done"),
            text("really done after tests"),
        ],
        vec![Arc::new(NamedTool {
            name: "edit".into(),
        })],
    );
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    // After first content-only turn, follow-up injects stop-gate message and
    // we sample again — so at least 2 model calls after the initial tool turn.
    assert!(
        calls.len() >= 3,
        "stop gate should force another turn after unverified edit: {} calls",
        calls.len()
    );
    let has_gate = calls.iter().any(|round| {
        round.iter().any(|m| {
            matches!(
                m,
                Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("STOP GATE") && t.contains("kind=stop_gate"))
            )
        })
    });
    assert!(has_gate, "expected STOP GATE follow-up in context: {calls:?}");
}

#[tokio::test]
async fn edit_thrash_blocks_after_repeated_failures() {
    struct FailEdit;
    #[async_trait]
    impl AgentTool for FailEdit {
        fn name(&self) -> &str {
            "edit"
        }
        fn description(&self) -> &str {
            "fail"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{"path":{"type":"string"}}})
        }
        async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
            anyhow::bail!("oldText not found")
        }
    }

    let (mut agent, _seen) = build(
        vec![
            tc("1", "edit", json!({"path": "f.rs"})),
            tc("2", "edit", json!({"path": "f.rs"})),
            tc("3", "edit", json!({"path": "f.rs"})),
            text("gave up"),
        ],
        vec![Arc::new(FailEdit)],
    );
    let new = agent.prompt("go").await.unwrap();
    let blocked = new.iter().any(|m| {
        matches!(
            m,
            Message::ToolResult(r) if r.is_error
                && r.content.iter().any(|b| b.as_text().is_some_and(|t|
                    t.contains("already failed")
                        || t.contains("fingerprint was already used")
                        || t.contains("already made")
                        || t.contains("loop detection")
                        || t.contains("identical args")
                ))
        )
    });
    assert!(
        blocked,
        "third failing edit on same path should be blocked: {new:?}"
    );
}

#[tokio::test]
async fn bash_failure_steers_different_command() {
    struct FailBash;
    #[async_trait]
    impl AgentTool for FailBash {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "fail"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{"command":{"type":"string"}}})
        }
        async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
            anyhow::bail!("bash: foo: command not found\nCommand exited with code 127")
        }
    }

    let (mut agent, seen) = build(
        vec![
            tc("1", "bash", json!({"command": "foo"})),
            text("trying something else"),
        ],
        vec![Arc::new(FailBash)],
    );
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    assert!(calls.len() >= 2, "cmd_fail should trigger a steered second turn");
    let has_cmd_fail = calls.iter().any(|round| {
        round.iter().any(|m| {
            matches!(
                m,
                Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t)
                    if t.contains("kind=cmd_fail") && t.contains("Do NOT re-run the same command"))
            )
        })
    });
    assert!(
        has_cmd_fail,
        "expected cmd_fail steering after bash error: {calls:?}"
    );
}

#[tokio::test]
async fn plan_pinned_at_tail_of_context() {
    let (mut agent, seen) = build(
        vec![
            tc("1", "update_plan", json!({"plan": "1. do x\n2. do y"})),
            text("working"),
        ],
        vec![],
    );
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    let last_call = calls.last().unwrap();
    let pin_pos = last_call.iter().position(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("<system-reminder>") && t.contains("1. do x"))
    ));
    assert!(
        pin_pos.is_some(),
        "plan pin should be in context: {last_call:?}"
    );
    assert_eq!(
        pin_pos.unwrap(),
        last_call.len() - 2,
        "pin sits before the last message"
    );

    let duplicates = last_call.iter().filter(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("<system-reminder>") && t.contains("kind=plan"))
    )).count();
    assert_eq!(duplicates, 1, "old pins must be replaced, not accumulated");
}

#[tokio::test]
async fn stop_gate_survives_plan_pin_after_unverified_edit() {
    // Regression: on_context must not strip stop_gate / verify reminders when
    // a plan is set — only kind=plan pins are replaced.
    let (mut agent, seen) = build(
        vec![
            tc("1", "update_plan", json!({"plan": "1. edit\n2. verify"})),
            tc("2", "edit", json!({"path": "f.rs"})),
            text("all done without tests"),
            text("really done after tests"),
        ],
        vec![Arc::new(NamedTool {
            name: "edit".into(),
        })],
    );
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    assert!(
        calls.len() >= 3,
        "stop gate should force another turn after plan+edit: {} calls",
        calls.len()
    );

    // After the unverified finish, some model-visible round must carry STOP GATE
    // *alongside* the plan pin (not replaced by it).
    let has_gate_with_plan = calls.iter().any(|round| {
        let texts: Vec<String> = round
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => match &u.content {
                    pirs_ai::UserContent::Text(t) => Some(t.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let has_gate = texts
            .iter()
            .any(|t| t.contains("STOP GATE") && t.contains("kind=stop_gate"));
        let has_plan = texts
            .iter()
            .any(|t| t.contains("kind=plan") && t.contains("1. edit"));
        has_gate && has_plan
    });
    assert!(
        has_gate_with_plan,
        "STOP GATE must remain model-visible after update_plan: {calls:?}"
    );

    // Plan pin still de-duped to one per round that has it.
    for round in calls.iter() {
        let plan_pins = round
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("kind=plan"))
                )
            })
            .count();
        assert!(
            plan_pins <= 1,
            "plan pins must not accumulate: {plan_pins} in {round:?}"
        );
    }
}

#[tokio::test]
async fn exec_runs_commands_with_timeout() {
    let mut host = ExtensionHost::new();
    host.load_source(
        r#"
register_tool("run", "run a command", #{ type: "object", properties: #{ cmd: #{ type: "string" } }, required: ["cmd"] });
fn tool_run(args) {
    let r = exec(args.cmd, 2);
    let out = r.output;
    out.trim();
    `code=${r.code} out=${out}`
}
"#,
        "exec_test.rhai".into(),
    )
    .unwrap();
    let host = Arc::new(host);
    let tools = host.tools();
    let out = tools[0]
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"cmd": "echo hello"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert_eq!(out.content[0].as_text().unwrap(), "code=0 out=hello");

    let out = tools[0]
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"cmd": "sleep 10"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert!(out.content[0].as_text().unwrap().contains("code=-1"));
}
