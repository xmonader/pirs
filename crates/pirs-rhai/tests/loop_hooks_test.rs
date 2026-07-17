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
        let text = msg.text();
        Box::pin(futures::stream::iter(vec![
            StreamEvent::Start,
            StreamEvent::TextDelta(text),
            StreamEvent::Done(Box::new(msg)),
        ]))
    }
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
        json!({"type":"object"})
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }
}

fn text_msg(t: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::text(t)],
        stop_reason: StopReason::Stop,
        ..Default::default()
    }
}

fn tool_call_msg() -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: json!({}),
            thought_signature: None,
        }],
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    }
}

fn build(script: &str, provider_msgs: Vec<AssistantMessage>) -> (Agent, Arc<Mutex<Vec<Vec<Message>>>>) {
    let mut host = ExtensionHost::new();
    host.load_source(script, "test.rhai".into()).unwrap();
    let host = Arc::new(host);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(MockProvider {
        scripted: Mutex::new(provider_msgs.into()),
        seen: Arc::clone(&seen),
    });

    let mut tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool)];
    tools.extend(host.tools());

    let mut agent = Agent::new(provider, "mock")
        .with_tools(tools)
        .with_hooks(host.hooks());
    if let Some(l) = host.listener() {
        agent.subscribe(l);
    }
    (agent, seen)
}

#[tokio::test]
async fn on_context_rewrites_outgoing_messages() {
    let script = r#"
fn on_context(messages) {
    messages.push(#{ role: "user", content: "injected-by-script", timestamp: 0 });
    messages
}
"#;
    let (mut agent, seen) = build(script, vec![text_msg("done")]);
    agent.prompt("go").await.unwrap();

    let first_call = &seen.lock().unwrap()[0];
    assert!(first_call.iter().any(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t == "injected-by-script")
    )));
}

#[tokio::test]
async fn on_should_stop_ends_loop_after_turn() {
    let script = r#"
fn on_should_stop(info) { true }
"#;
    let (mut agent, seen) = build(script, vec![tool_call_msg(), text_msg("never-reached")]);
    agent.prompt("go").await.unwrap();

    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(matches!(
        agent.messages.last(),
        Some(Message::ToolResult(_))
    ));
}

#[tokio::test]
async fn on_steering_injects_message_mid_run() {
    let script = r#"
fn on_steering() {
    if state_has("done") {
        ()
    } else {
        state_set("done", true);
        "steered-by-script"
    }
}
"#;
    let (mut agent, seen) = build(script, vec![text_msg("one"), text_msg("two")]);
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().any(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t == "steered-by-script")
    )));
}

#[tokio::test]
async fn on_follow_up_continues_otherwise_finished_run() {
    let script = r#"
fn on_follow_up() {
    if state_has("done") {
        ()
    } else {
        state_set("done", true);
        ["again!"]
    }
}
"#;
    let (mut agent, seen) = build(script, vec![text_msg("first"), text_msg("second")]);
    agent.prompt("go").await.unwrap();

    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].iter().any(|m| matches!(
        m,
        Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t == "again!")
    )));
    assert!(matches!(
        agent.messages.last(),
        Some(Message::Assistant(a)) if a.text() == "second"
    ));
}

#[test]
fn listener_only_when_on_event_defined() {
    let mut host = ExtensionHost::new();
    host.load_source("fn on_event(t, d) { }", "a.rhai".into())
        .unwrap();
    let host = Arc::new(host);
    assert!(host.listener().is_some());

    let host2 = Arc::new(ExtensionHost::new());
    assert!(host2.listener().is_none());
}

#[test]
fn hooks_empty_when_no_loop_functions() {
    let host = Arc::new(ExtensionHost::new());
    let hooks = host.hooks();
    assert!(hooks.transform_context.is_none());
    assert!(hooks.should_stop_after_turn.is_none());
    assert!(hooks.get_steering_messages.is_none());
    assert!(hooks.get_follow_up_messages.is_none());
}
