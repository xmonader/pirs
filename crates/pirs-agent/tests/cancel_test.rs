use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::Agent;
use pirs_ai::{AssistantMessage, CompletionOptions, Context, LlmProvider, StopReason, StreamEvent};

/// Provider that pends until its cancel token fires, then reports an abort.
/// Models a long-running generation so tests can cancel mid-flight.
struct BlockingProvider;

#[async_trait]
impl LlmProvider for BlockingProvider {
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

/// A cancel handle captured BEFORE prompt() must still cancel the run:
/// begin_prompt re-mints the run token inside a stable slot, and cancelling
/// through the slot must reach the current run. Regression test for the
/// stale-handle bug that broke Ctrl-C, delegate watchers, and the TUI.
#[tokio::test]
async fn cancel_handle_captured_before_prompt_cancels_run() {
    let mut agent = Agent::new(Arc::new(BlockingProvider), "mock-model");
    let handle = agent.cancel_handle();

    let run = agent
        .begin_prompt(vec![pirs_ai::Message::user("hi")])
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    handle.lock().unwrap().cancel();

    let (full, _new, _hit) = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("run did not finish after cancel — handle is stale");
    agent.complete_run(full);
}

/// agent.cancel() cancels the active run and the agent stays usable: a second
/// prompt afterwards runs to completion (slot is re-minted per run).
#[tokio::test]
async fn agent_remains_usable_after_cancel() {
    let mut agent = Agent::new(Arc::new(BlockingProvider), "mock-model");
    let run = agent
        .begin_prompt(vec![pirs_ai::Message::user("hi")])
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    agent.cancel();
    let (full, _new, _hit) = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("first run did not finish after cancel");
    agent.complete_run(full);

    // Second run must start fresh (not born-cancelled) and be cancellable too.
    let run2 = agent
        .begin_prompt(vec![pirs_ai::Message::user("again")])
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    agent.cancel();
    let (full2, _new2, _hit2) = tokio::time::timeout(std::time::Duration::from_secs(5), run2)
        .await
        .expect("second run was born cancelled or not cancellable");
    agent.complete_run(full2);
}
