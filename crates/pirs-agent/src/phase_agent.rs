//! Running a loop [`Strategy`](crate::strategy::Strategy) on the real product
//! [`Agent`] — the bridge that makes `--strategy`/`--profile` first-class.
//!
//! The bench harness has its own [`PhaseDriver`](crate::strategy::PhaseDriver)
//! wired to a git workspace and verification. The product agent needs a driver
//! that runs each phase as an ordinary agent turn instead. This one owns nothing
//! but a *factory*: given a rendered [`PhaseReq`], the caller builds a fully
//! configured [`Agent`] (its tools scoped to the phase, its model, and the same
//! hooks, listeners, and session wiring the plain one-shot path uses). The driver
//! prompts that agent and hands its final text to the next phase's `{prev}`.
//!
//! Keeping the factory on the caller side means this module stays ignorant of
//! approval policy, printing, tool taxonomy, and session persistence — the caller
//! already owns all of that for the naive loop and simply reuses it per phase.

use crate::agent::Agent;
use crate::strategy::{AsyncPhaseDriver, PhaseReq};
use pirs_ai::Message;

/// The last non-empty assistant text in a turn's messages — a phase's output.
fn last_assistant_text(msgs: &[Message]) -> String {
    msgs.iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => {
                let t = a.text();
                (!t.trim().is_empty()).then_some(t)
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// Drives strategy phases on freshly built product [`Agent`]s.
///
/// `make_agent` is called once per phase with the rendered [`PhaseReq`]; it must
/// return an [`Agent`] configured for that phase (scoped tools, phase system
/// prompt, phase/default model, plus the caller's shared hooks/listeners). A
/// fresh agent per phase is exactly the split-strategy contract: the executor
/// starts clean, seeded only by the plan carried in its prompt.
pub struct AgentPhaseDriver<F> {
    make_agent: F,
}

impl<F> AgentPhaseDriver<F>
where
    F: FnMut(&PhaseReq) -> Agent,
{
    pub fn new(make_agent: F) -> Self {
        AgentPhaseDriver { make_agent }
    }
}

#[async_trait::async_trait(?Send)]
impl<F> AsyncPhaseDriver for AgentPhaseDriver<F>
where
    F: FnMut(&PhaseReq) -> Agent,
{
    async fn run_phase(&mut self, req: &PhaseReq) -> anyhow::Result<String> {
        let mut agent = (self.make_agent)(req);
        let msgs = agent
            .prompt(req.prompt.clone())
            .await
            .map_err(|e| anyhow::anyhow!("phase {} failed: {e}", req.phase_id))?;
        Ok(last_assistant_text(&msgs))
    }

    /// Fan-out: build every branch's agent first (the factory borrows `self`),
    /// then await all the prompts together. The prompt futures borrow the local
    /// `agents` vector, not `self`, so concurrency doesn't alias the driver. The
    /// branches are read-only by strategy contract, so nothing races on the tree.
    async fn run_parallel(&mut self, reqs: &[PhaseReq]) -> Vec<anyhow::Result<String>> {
        let mut agents: Vec<(String, Agent, String)> = reqs
            .iter()
            .map(|req| {
                let agent = (self.make_agent)(req);
                (req.phase_id.clone(), agent, req.prompt.clone())
            })
            .collect();

        let futs = agents.iter_mut().map(|(id, agent, prompt)| async move {
            agent
                .prompt(prompt.clone())
                .await
                .map(|msgs| last_assistant_text(&msgs))
                .map_err(|e| anyhow::anyhow!("phase {id} failed: {e}"))
        });
        futures::future::join_all(futs).await
    }
}
