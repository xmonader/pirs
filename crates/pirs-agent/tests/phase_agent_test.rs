//! `AgentPhaseDriver` end to end: driving real `Agent`s through a strategy with
//! a scripted streaming provider, proving per-phase system prompts, per-phase
//! model overrides, and `{prev}` threading all reach the model correctly.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::phase_agent::AgentPhaseDriver;
use pirs_agent::strategy::{
    run_strategy_async, Join, Phase, PhaseReq, Step, Strategy, Task, ToolScope,
};
use pirs_agent::Agent;

// Minimal strategy fixtures (the built-in content lives in pirs-rhai; this test
// only needs well-shaped strategies to drive the AgentPhaseDriver).
fn ph(system: &str, prompt: &str, scope: ToolScope, model: Option<&str>) -> Phase {
    Phase {
        system: system.into(),
        prompt: prompt.into(),
        scope,
        model: model.map(String::from),
    }
}
fn plan_oracle_exec(oracle_model: &str) -> Strategy {
    Strategy {
        name: "plan-oracle-exec".into(),
        persist_across_attempts: false,
        steps: vec![
            Step::Solo(ph("plan-sys", "plan {issue}", ToolScope::ReadOnly, None)),
            Step::Solo(ph(
                "critic-sys",
                "critic {prev}",
                ToolScope::ReadOnly,
                Some(oracle_model),
            )),
            Step::Solo(ph("exec-sys", "exec {prev}", ToolScope::Full, None)),
        ],
    }
}
fn wide_plan_exec(n: usize) -> Strategy {
    Strategy {
        name: "wide-plan-exec".into(),
        persist_across_attempts: false,
        steps: vec![
            Step::Fan {
                branches: (0..n)
                    .map(|_| ph("plan-sys", "plan {issue}", ToolScope::ReadOnly, None))
                    .collect(),
                join: Join::Concat,
            },
            Step::Solo(ph("exec-sys", "exec {prev}", ToolScope::Full, None)),
        ],
    }
}
use pirs_ai::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, Message, StopReason,
    StreamEvent,
};

/// One recorded provider call: the phase's system prompt, the last user text it
/// was prompted with, and the model it ran on.
#[derive(Clone, Debug)]
struct Seen {
    system: Option<String>,
    user: String,
    model: String,
}

/// A streaming provider that records each call and replies with a scripted text
/// (or "done" once the script runs out). Shared across every phase's agent so we
/// observe the whole run in order.
struct RecordingProvider {
    scripted: Mutex<std::collections::VecDeque<String>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl RecordingProvider {
    fn new(scripted: Vec<&str>) -> Self {
        RecordingProvider {
            scripted: Mutex::new(scripted.into_iter().map(String::from).collect()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

fn last_user_text(ctx: &Context) -> String {
    ctx.messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(u) => match &u.content {
                pirs_ai::UserContent::Text(t) => Some(t.clone()),
                _ => Some(String::new()),
            },
            _ => None,
        })
        .unwrap_or_default()
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn stream(
        &self,
        model: &str,
        context: &Context,
        _options: &CompletionOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures::stream::BoxStream<'static, StreamEvent> {
        self.seen.lock().unwrap().push(Seen {
            system: context.system_prompt.clone(),
            user: last_user_text(context),
            model: model.to_string(),
        });
        let text = self
            .scripted
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "done".to_string());
        let msg = AssistantMessage {
            content: vec![ContentBlock::text(&text)],
            stop_reason: StopReason::Stop,
            ..Default::default()
        };
        let events = vec![
            StreamEvent::Start,
            StreamEvent::TextDelta(text),
            StreamEvent::Done(Box::new(msg)),
        ];
        Box::pin(futures::stream::iter(events))
    }
}

fn task() -> Task {
    Task {
        issue: "the widget crashes on empty input".into(),
        targets: vec!["tests/test_widget.py::test_empty".into()],
        verdict: None,
    }
}

/// A factory building a fresh, compaction-free agent per phase, scoping model to
/// the phase override (the Oracle lever) and stamping the phase system prompt.
fn driver_over(
    provider: Arc<RecordingProvider>,
) -> AgentPhaseDriver<impl FnMut(&PhaseReq) -> Agent> {
    AgentPhaseDriver::new(move |req: &PhaseReq| {
        let model = req.model.clone().unwrap_or_else(|| "default-model".into());
        Agent::new(Arc::clone(&provider) as Arc<dyn LlmProvider>, model)
            .with_system_prompt(req.system.clone())
            .with_compaction(None)
    })
}

#[tokio::test]
async fn oracle_strategy_routes_system_prompts_models_and_prev() {
    let provider = Arc::new(RecordingProvider::new(vec![
        "PLAN_ALPHA",
        "CRITIQUE_BETA",
        "EXEC_DONE",
    ]));
    let seen = Arc::clone(&provider.seen);
    let mut driver = driver_over(provider);

    // plan -> critic(strong-model) -> exec
    run_strategy_async(&plan_oracle_exec("strong-model"), &mut driver, &task())
        .await
        .unwrap();

    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 3, "three phases, three model calls");

    // Each phase carries its own distinct system prompt.
    assert_ne!(
        calls[0].system, calls[2].system,
        "plan vs exec system differ"
    );
    assert!(calls[0].system.is_some());

    // The Oracle lever: only the critic phase runs on the stronger model.
    assert_eq!(calls[0].model, "default-model");
    assert_eq!(calls[1].model, "strong-model");
    assert_eq!(calls[2].model, "default-model");

    // {prev} threading through real agent turns: the critic saw the plan's output,
    // and the executor saw the critic's output.
    assert!(
        calls[1].user.contains("PLAN_ALPHA"),
        "critic prompt missing plan output: {}",
        calls[1].user
    );
    assert!(
        calls[2].user.contains("CRITIQUE_BETA"),
        "exec prompt missing critic output: {}",
        calls[2].user
    );
}

#[tokio::test]
async fn fan_out_runs_every_branch_then_merges_for_the_executor() {
    // 3 planners + 1 executor; leave the script empty so every call returns "done".
    let provider = Arc::new(RecordingProvider::new(vec![]));
    let seen = Arc::clone(&provider.seen);
    let mut driver = driver_over(provider);

    run_strategy_async(&wide_plan_exec(3), &mut driver, &task())
        .await
        .unwrap();

    let calls = seen.lock().unwrap();
    assert_eq!(calls.len(), 4, "3 parallel planners + 1 executor");

    // The executor is the last call; its prompt merges all three branch outputs
    // under `## Branch N` headings.
    let exec_prompt = &calls[3].user;
    assert!(exec_prompt.contains("## Branch 1"), "{exec_prompt}");
    assert!(exec_prompt.contains("## Branch 2"), "{exec_prompt}");
    assert!(exec_prompt.contains("## Branch 3"), "{exec_prompt}");
}
