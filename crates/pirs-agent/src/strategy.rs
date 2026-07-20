//! Loop strategies — how an agent's work is *structured* into phases.
//!
//! A [`Strategy`] is plain data: an ordered list of [`Phase`]s, each a system
//! prompt, a prompt template, and a [`ToolScope`]. The engine ([`run_strategy`])
//! walks the phases, renders each template against the [`Task`] (issue, targets,
//! the prior phase's output, the last verdict), and drives it through a
//! [`PhaseDriver`] — the actual agent loop, supplied by the caller.
//!
//! This split is deliberate:
//! - The **policy** (which phases, what prompts, which tools) is data. A
//!   [`Strategy`] is just a value; the built-ins ship as embedded `.rhai` scripts
//!   in `pirs-rhai` (`builtins`), and a user can author their own — from a script
//!   or in Rust — without touching this engine.
//! - The **engine** (running a sub-loop, managing context, token/behaviour
//!   accounting) is a [`PhaseDriver`] the host implements once.
//!
//! This module is pure mechanism: the [`Strategy`]/[`Phase`]/[`Step`] data types
//! and the runner. It defines no built-in strategies. Nothing here is
//! benchmark-specific; the bench harness is one consumer.

/// Which tools a phase may use. The read-only scope is how a planning phase is
/// prevented from mutating the tree — it never sees edit/write/shell tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolScope {
    /// Read/search/navigate only — no tool that can change files.
    ReadOnly,
    /// The full tool set, including edit/write/shell.
    Full,
}

/// One phase of a strategy: a fresh (or continued) sub-conversation.
#[derive(Debug, Clone)]
pub struct Phase {
    /// System prompt establishing the phase's role.
    pub system: String,
    /// Prompt template. Placeholders: `{issue}`, `{targets}`, `{prev}` (the
    /// previous phase's text output), `{verdict}` (the last attempt's verdict, or
    /// empty on the first attempt).
    pub prompt: String,
    pub scope: ToolScope,
    /// Model override for this phase. `None` uses the run's default model. This is
    /// the "Oracle" lever: run e.g. the critic phase on a stronger reasoning model
    /// than the executor — a *different* model for the second opinion.
    pub model: Option<String>,
}

/// How a fan-out step merges its branches' outputs into the `{prev}` of the next
/// step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// Concatenate all branch outputs, each under a `## Branch N` heading.
    Concat,
    /// Keep only the first branch's output (e.g. fastest-wins / primary).
    First,
}

/// One step of a strategy: a single phase, or a fan-out of branches run
/// concurrently whose outputs are merged. Fan-out is how independent research or
/// competing-hypothesis exploration runs in parallel.
#[derive(Debug, Clone)]
pub enum Step {
    Solo(Phase),
    Fan { branches: Vec<Phase>, join: Join },
}

/// A named, ordered sequence of steps.
#[derive(Debug, Clone)]
pub struct Strategy {
    pub name: String,
    pub steps: Vec<Step>,
    /// When true, the (single) phase's context is reused across attempts so the
    /// agent accumulates memory — the "monolithic" baseline. When false, every
    /// phase starts from a clean context each attempt (the plan/execute split,
    /// whose whole point is a fresh executor seeded with only the plan).
    pub persist_across_attempts: bool,
}

/// A fully-rendered phase ready to run: the engine has already substituted the
/// prompt template, so the driver only executes it. Owned so it can move into a
/// concurrent task.
#[derive(Debug, Clone)]
pub struct PhaseReq {
    pub phase_id: String,
    pub system: String,
    pub prompt: String,
    pub scope: ToolScope,
    pub fresh: bool,
    pub model: Option<String>,
}

/// The task a strategy is run against.
#[derive(Debug, Clone)]
pub struct Task {
    pub issue: String,
    pub targets: Vec<String>,
    /// The prior attempt's gate verdict, rendered for `{verdict}`. `None` on the
    /// first attempt.
    pub verdict: Option<String>,
}

/// The engine behind a phase: run one agent sub-loop and return its text output.
///
/// The host implements this once (owning the provider, tool sets, context, and
/// token/behaviour accounting). `phase_id` identifies the phase for context
/// continuity; `fresh` forces a new context, dropping any prior messages for that
/// id. `scope` selects the tool set.
pub trait PhaseDriver {
    /// Run one fully-rendered phase and return its text output.
    fn run_phase(&mut self, req: &PhaseReq) -> anyhow::Result<String>;

    /// Run several phases concurrently, one result per request in order. The
    /// default runs them sequentially; a host with a runtime should override this
    /// to dispatch in parallel. Fan-out branches are read-only by contract, so
    /// concurrent execution cannot race on the tree.
    fn run_parallel(&mut self, reqs: &[PhaseReq]) -> Vec<anyhow::Result<String>> {
        reqs.iter().map(|r| self.run_phase(r)).collect()
    }
}

/// Render a bulleted list of test targets.
fn render_targets(targets: &[String]) -> String {
    let mut s = String::new();
    for t in targets {
        s.push_str("- ");
        s.push_str(t);
        s.push('\n');
    }
    s
}

/// Render the `{verdict}` preamble: a retry note when a prior verdict exists,
/// empty otherwise.
fn render_verdict(verdict: Option<&str>) -> String {
    match verdict {
        Some(v) => format!(
            "A previous attempt did not pass (gate verdict: {v}). \
             Re-examine against the failing test and correct the fix.\n\n"
        ),
        None => String::new(),
    }
}

/// Substitute a phase template's placeholders. `prev` is the previous phase's
/// output (empty for the first phase).
pub fn render(template: &str, task: &Task, prev: &str) -> String {
    template
        .replace("{issue}", task.issue.trim())
        .replace("{targets}", render_targets(&task.targets).trim_end())
        .replace("{prev}", prev.trim())
        .replace("{verdict}", &render_verdict(task.verdict.as_deref()))
}

/// Run a strategy end to end for one attempt: each phase's rendered prompt is
/// driven in order, and each phase's text output feeds `{prev}` of the next. The
/// phases' side effects (edits) land on the tree via the driver; this returns
/// nothing but the driver's errors.
pub fn run_strategy(
    strategy: &Strategy,
    driver: &mut dyn PhaseDriver,
    task: &Task,
) -> anyhow::Result<()> {
    // Persistent strategies continue their context across attempts; split
    // strategies always start each phase clean.
    let fresh = !strategy.persist_across_attempts;
    let mut prev = String::new();
    for (i, step) in strategy.steps.iter().enumerate() {
        match step {
            Step::Solo(phase) => {
                let id = format!("{}#{i}", strategy.name);
                let req = req_for(id, phase, task, &prev, fresh);
                prev = driver.run_phase(&req)?;
            }
            Step::Fan { branches, join } => {
                let reqs: Vec<PhaseReq> = branches
                    .iter()
                    .enumerate()
                    .map(|(b, phase)| {
                        let id = format!("{}#{i}.{b}", strategy.name);
                        req_for(id, phase, task, &prev, fresh)
                    })
                    .collect();
                let results = driver.run_parallel(&reqs);
                // Tolerate partial branch failure: merge the successes; only error
                // if the whole fan-out came up empty.
                let outs: Vec<String> = results.into_iter().filter_map(|r| r.ok()).collect();
                if outs.is_empty() {
                    anyhow::bail!("all parallel branches failed at step {i}");
                }
                prev = merge(*join, &outs);
            }
        }
    }
    Ok(())
}

/// The async twin of [`PhaseDriver`], for hosts that are already inside a Tokio
/// runtime (the interactive/one-shot product agent) and cannot block on it.
/// Same contract as [`PhaseDriver`]; drive it with [`run_strategy_async`]. The
/// futures are not required to be `Send`: the runner is awaited directly on the
/// product agent's task, never spawned onto another thread.
#[async_trait::async_trait(?Send)]
pub trait AsyncPhaseDriver {
    /// Run one fully-rendered phase and return its text output.
    async fn run_phase(&mut self, req: &PhaseReq) -> anyhow::Result<String>;

    /// Run several phases concurrently, one result per request in order. The
    /// default awaits them sequentially; a host should override to dispatch the
    /// (read-only) branches at once.
    async fn run_parallel(&mut self, reqs: &[PhaseReq]) -> Vec<anyhow::Result<String>> {
        let mut out = Vec::with_capacity(reqs.len());
        for r in reqs {
            out.push(self.run_phase(r).await);
        }
        out
    }
}

/// Async counterpart of [`run_strategy`]: identical phase-walking and `{prev}`
/// threading, but awaited so it composes with an existing runtime. Used by the
/// product agent to run a `--strategy` on the real agent loop.
pub async fn run_strategy_async(
    strategy: &Strategy,
    driver: &mut dyn AsyncPhaseDriver,
    task: &Task,
) -> anyhow::Result<()> {
    let fresh = !strategy.persist_across_attempts;
    let mut prev = String::new();
    for (i, step) in strategy.steps.iter().enumerate() {
        match step {
            Step::Solo(phase) => {
                let id = format!("{}#{i}", strategy.name);
                let req = req_for(id, phase, task, &prev, fresh);
                prev = driver.run_phase(&req).await?;
            }
            Step::Fan { branches, join } => {
                let reqs: Vec<PhaseReq> = branches
                    .iter()
                    .enumerate()
                    .map(|(b, phase)| {
                        let id = format!("{}#{i}.{b}", strategy.name);
                        req_for(id, phase, task, &prev, fresh)
                    })
                    .collect();
                let results = driver.run_parallel(&reqs).await;
                let outs: Vec<String> = results.into_iter().filter_map(|r| r.ok()).collect();
                if outs.is_empty() {
                    anyhow::bail!("all parallel branches failed at step {i}");
                }
                prev = merge(*join, &outs);
            }
        }
    }
    Ok(())
}

/// Build a rendered request for one phase under the given `phase_id`.
fn req_for(phase_id: String, phase: &Phase, task: &Task, prev: &str, fresh: bool) -> PhaseReq {
    PhaseReq {
        phase_id,
        system: phase.system.clone(),
        prompt: render(&phase.prompt, task, prev),
        scope: phase.scope,
        fresh,
        model: phase.model.clone(),
    }
}

/// Merge a fan-out step's branch outputs per its [`Join`].
fn merge(join: Join, outs: &[String]) -> String {
    match join {
        Join::First => outs.first().cloned().unwrap_or_default(),
        Join::Concat => outs
            .iter()
            .enumerate()
            .map(|(i, o)| format!("## Branch {}\n{}", i + 1, o.trim()))
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Pin a strong planner (and critic) model onto every **read-only** phase.
///
/// Full-scope executor phases are left alone so they keep the run default
/// (`--model` / weak executor). This is the product multi-model pitch:
/// `--model <cheap> --plan-model <strong> --strategy plan-exec|plan-critic-exec`.
///
/// Applied after profile resolution so it overrides a profile-wide default on
/// plan/critic phases only.
pub fn pin_plan_model(strategy: &mut Strategy, plan_model: &str) {
    let model = plan_model.to_string();
    for step in &mut strategy.steps {
        match step {
            Step::Solo(phase) if phase.scope == ToolScope::ReadOnly => {
                phase.model = Some(model.clone());
            }
            Step::Fan { branches, .. } => {
                for phase in branches {
                    if phase.scope == ToolScope::ReadOnly {
                        phase.model = Some(model.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every phase it is asked to run, and returns a canned output so the
    /// next phase's `{prev}` can be checked.
    #[derive(Default)]
    struct RecordingDriver {
        calls: Vec<(String, ToolScope, bool, String, Option<String>)>,
        /// phase_ids that should fail — used to exercise partial fan-out failure.
        fail: Vec<String>,
    }
    impl PhaseDriver for RecordingDriver {
        fn run_phase(&mut self, req: &PhaseReq) -> anyhow::Result<String> {
            self.calls.push((
                req.phase_id.clone(),
                req.scope,
                req.fresh,
                req.prompt.clone(),
                req.model.clone(),
            ));
            if self.fail.contains(&req.phase_id) {
                anyhow::bail!("forced failure for {}", req.phase_id);
            }
            // Return a phase-specific marker to trace it into the next {prev}.
            Ok(format!("OUTPUT_OF[{}]", req.phase_id))
        }
    }

    fn task() -> Task {
        Task {
            issue: "add() subtracts".into(),
            targets: vec!["t.py::test_add".into()],
            verdict: None,
        }
    }

    // Minimal fixtures matching the *shape* (names, phase count, scopes, model
    // routing) of the built-ins the engine tests exercise. The built-in *content*
    // lives in pirs-rhai now; these tests only need a well-shaped strategy.
    fn ph(system: &str, prompt: &str, scope: ToolScope) -> Phase {
        Phase {
            system: system.into(),
            prompt: prompt.into(),
            scope,
            model: None,
        }
    }
    fn monolithic() -> Strategy {
        Strategy {
            name: "monolithic".into(),
            persist_across_attempts: true,
            steps: vec![Step::Solo(ph(
                "mono",
                "fix {issue}\n{targets}",
                ToolScope::Full,
            ))],
        }
    }
    fn plan_exec() -> Strategy {
        Strategy {
            name: "plan-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                Step::Solo(ph("plan", "plan {issue}\n{targets}", ToolScope::ReadOnly)),
                Step::Solo(ph("exec", "exec {prev}", ToolScope::Full)),
            ],
        }
    }
    fn plan_critic_exec() -> Strategy {
        Strategy {
            name: "plan-critic-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                Step::Solo(ph("plan", "plan {issue}", ToolScope::ReadOnly)),
                Step::Solo(ph("critic", "critic {prev}", ToolScope::ReadOnly)),
                Step::Solo(ph("exec", "exec {prev}", ToolScope::Full)),
            ],
        }
    }
    fn plan_oracle_exec(oracle_model: &str) -> Strategy {
        let mut critic = ph("critic", "critic {prev}", ToolScope::ReadOnly);
        critic.model = Some(oracle_model.to_string());
        Strategy {
            name: "plan-oracle-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                Step::Solo(ph("plan", "plan {issue}", ToolScope::ReadOnly)),
                Step::Solo(critic),
                Step::Solo(ph("exec", "exec {prev}", ToolScope::Full)),
            ],
        }
    }
    fn wide_plan_exec(n: usize) -> Strategy {
        let branches = (0..n)
            .map(|i| ph(&format!("plan{i}"), "plan {issue}", ToolScope::ReadOnly))
            .collect();
        Strategy {
            name: "wide-plan-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                Step::Fan {
                    branches,
                    join: Join::Concat,
                },
                Step::Solo(ph("exec", "exec {prev}", ToolScope::Full)),
            ],
        }
    }

    #[test]
    fn render_substitutes_all_placeholders() {
        let t = Task {
            verdict: Some("NotYet".into()),
            ..task()
        };
        let out = render(
            "{verdict}fix {issue} for\n{targets}\nprior: {prev}",
            &t,
            "THE PLAN",
        );
        assert!(out.contains("add() subtracts"));
        assert!(out.contains("- t.py::test_add"));
        assert!(out.contains("prior: THE PLAN"));
        assert!(out.contains("gate verdict: NotYet"));
    }

    #[test]
    fn monolithic_runs_one_full_persistent_phase() {
        let mut d = RecordingDriver::default();
        run_strategy(&monolithic(), &mut d, &task()).unwrap();
        assert_eq!(d.calls.len(), 1);
        let (id, scope, fresh, prompt, _model) = &d.calls[0];
        assert_eq!(id, "monolithic#0");
        assert_eq!(*scope, ToolScope::Full);
        assert!(!*fresh, "monolithic persists context across attempts");
        assert!(prompt.contains("add() subtracts"));
    }

    #[test]
    fn plan_exec_plans_read_only_then_executes_with_the_plan() {
        let mut d = RecordingDriver::default();
        run_strategy(&plan_exec(), &mut d, &task()).unwrap();
        assert_eq!(d.calls.len(), 2);
        // Phase 0: planning, read-only, fresh.
        assert_eq!(d.calls[0].1, ToolScope::ReadOnly);
        assert!(d.calls[0].2, "split phases start fresh");
        // Phase 1: execution, full tools, and it received the plan's output.
        assert_eq!(d.calls[1].1, ToolScope::Full);
        assert!(
            d.calls[1].3.contains("OUTPUT_OF[plan-exec#0]"),
            "executor prompt must embed the planner's output: {}",
            d.calls[1].3
        );
    }

    #[test]
    fn plan_critic_exec_inserts_a_read_only_critic_between_plan_and_exec() {
        let mut d = RecordingDriver::default();
        run_strategy(&plan_critic_exec(), &mut d, &task()).unwrap();
        assert_eq!(d.calls.len(), 3);
        assert_eq!(d.calls[1].1, ToolScope::ReadOnly); // critic reads, doesn't edit
        assert!(d.calls[1].3.contains("OUTPUT_OF[plan-critic-exec#0]")); // sees plan
        assert!(d.calls[2].3.contains("OUTPUT_OF[plan-critic-exec#1]")); // exec sees vetted plan
        assert_eq!(d.calls[2].1, ToolScope::Full);
    }

    #[test]
    fn oracle_runs_the_critic_phase_on_a_different_model() {
        let mut d = RecordingDriver::default();
        run_strategy(&plan_oracle_exec("strong-model"), &mut d, &task()).unwrap();
        assert_eq!(d.calls.len(), 3);
        // Plan and exec use the default (None); only the critic overrides.
        assert_eq!(d.calls[0].4, None);
        assert_eq!(d.calls[1].4, Some("strong-model".to_string()));
        assert_eq!(d.calls[2].4, None);
    }

    #[test]
    fn pin_plan_model_strong_plan_weak_exec() {
        // Product pitch: plan (+ critic) on strong, executor keeps None → --model.
        let mut s = plan_critic_exec();
        pin_plan_model(&mut s, "strong-planner");
        match (&s.steps[0], &s.steps[1], &s.steps[2]) {
            (Step::Solo(plan), Step::Solo(critic), Step::Solo(exec)) => {
                assert_eq!(plan.model.as_deref(), Some("strong-planner"));
                assert_eq!(critic.model.as_deref(), Some("strong-planner"));
                assert_eq!(exec.model, None, "executor stays on run default");
                assert_eq!(exec.scope, ToolScope::Full);
            }
            _ => panic!("expected three solo phases"),
        }
        let mut d = RecordingDriver::default();
        run_strategy(&s, &mut d, &task()).unwrap();
        assert_eq!(d.calls[0].4.as_deref(), Some("strong-planner"));
        assert_eq!(d.calls[1].4.as_deref(), Some("strong-planner"));
        assert_eq!(d.calls[2].4, None);
    }

    #[test]
    fn pin_plan_model_leaves_monolithic_untouched() {
        let mut s = monolithic();
        pin_plan_model(&mut s, "strong");
        match &s.steps[0] {
            Step::Solo(p) => assert_eq!(p.model, None, "full-scope phase is not a planner"),
            _ => panic!("expected solo"),
        }
    }

    /// A driver whose `run_parallel` marks each branch's output so we can prove the
    /// concurrent path (not the sequential fallback) actually ran.
    #[derive(Default)]
    struct ParallelDriver {
        parallel_widths: Vec<usize>,
    }
    impl PhaseDriver for ParallelDriver {
        fn run_phase(&mut self, req: &PhaseReq) -> anyhow::Result<String> {
            Ok(format!("SOLO[{}]", req.phase_id))
        }
        fn run_parallel(&mut self, reqs: &[PhaseReq]) -> Vec<anyhow::Result<String>> {
            self.parallel_widths.push(reqs.len());
            reqs.iter()
                .map(|r| Ok(format!("PAR[{}]", r.phase_id)))
                .collect()
        }
    }

    #[test]
    fn wide_plan_exec_fans_out_then_executes_on_the_merged_plan() {
        let mut d = RecordingDriver::default();
        run_strategy(&wide_plan_exec(3), &mut d, &task()).unwrap();
        // 3 parallel planners + 1 executor.
        assert_eq!(d.calls.len(), 4);
        // Branch ids carry the `.b` suffix; all read-only.
        assert_eq!(d.calls[0].0, "wide-plan-exec#0.0");
        assert_eq!(d.calls[2].0, "wide-plan-exec#0.2");
        assert!(d.calls[..3].iter().all(|c| c.1 == ToolScope::ReadOnly));
        // The executor is full-scope and its prompt embeds every branch, merged
        // under `## Branch N` headings.
        let exec = &d.calls[3];
        assert_eq!(exec.1, ToolScope::Full);
        assert!(exec.3.contains("## Branch 1"));
        assert!(exec.3.contains("OUTPUT_OF[wide-plan-exec#0.0]"));
        assert!(exec.3.contains("OUTPUT_OF[wide-plan-exec#0.2]"));
    }

    #[test]
    fn fan_out_uses_the_drivers_parallel_path() {
        let mut d = ParallelDriver::default();
        run_strategy(&wide_plan_exec(2), &mut d, &task()).unwrap();
        // One fan-out step of width 2 went through run_parallel, not run_phase.
        assert_eq!(d.parallel_widths, vec![2]);
    }

    #[test]
    fn fan_out_tolerates_partial_branch_failure() {
        let mut d = RecordingDriver {
            fail: vec!["wide-plan-exec#0.1".into()],
            ..Default::default()
        };
        run_strategy(&wide_plan_exec(3), &mut d, &task()).unwrap();
        // Executor still ran; its merged plan contains the two surviving branches
        // and not the failed one.
        let exec = &d.calls.last().unwrap().3;
        assert!(exec.contains("OUTPUT_OF[wide-plan-exec#0.0]"));
        assert!(exec.contains("OUTPUT_OF[wide-plan-exec#0.2]"));
        assert!(!exec.contains("wide-plan-exec#0.1]"));
    }

    #[test]
    fn fan_out_fails_only_when_all_branches_fail() {
        let mut d = RecordingDriver {
            fail: vec!["wide-plan-exec#0.0".into(), "wide-plan-exec#0.1".into()],
            ..Default::default()
        };
        let err = run_strategy(&wide_plan_exec(2), &mut d, &task()).unwrap_err();
        assert!(err.to_string().contains("all parallel branches failed"));
    }

    #[test]
    fn merge_first_keeps_only_the_primary_branch() {
        let outs = vec!["A".to_string(), "B".to_string()];
        assert_eq!(merge(Join::First, &outs), "A");
        let concat = merge(Join::Concat, &outs);
        assert!(concat.contains("## Branch 1\nA"));
        assert!(concat.contains("## Branch 2\nB"));
    }

    /// Async driver mirroring `RecordingDriver`, to prove `run_strategy_async`
    /// walks phases and threads `{prev}` exactly like the sync engine.
    #[derive(Default)]
    struct AsyncRecorder {
        calls: Vec<(String, String)>, // (phase_id, rendered prompt)
    }
    #[async_trait::async_trait(?Send)]
    impl AsyncPhaseDriver for AsyncRecorder {
        async fn run_phase(&mut self, req: &PhaseReq) -> anyhow::Result<String> {
            self.calls.push((req.phase_id.clone(), req.prompt.clone()));
            Ok(format!("OUT[{}]", req.phase_id))
        }
    }

    #[tokio::test]
    async fn async_runner_threads_prev_through_phases() {
        let mut d = AsyncRecorder::default();
        run_strategy_async(&plan_exec(), &mut d, &task())
            .await
            .unwrap();
        assert_eq!(d.calls.len(), 2);
        // The executor phase's prompt must carry the planner's output via {prev}.
        assert!(
            d.calls[1].1.contains("OUT[plan-exec#0]"),
            "exec prompt missing prev: {}",
            d.calls[1].1
        );
    }

    #[tokio::test]
    async fn async_runner_fans_out_and_merges() {
        let mut d = AsyncRecorder::default();
        run_strategy_async(&wide_plan_exec(3), &mut d, &task())
            .await
            .unwrap();
        // 3 planners (default sequential run_parallel) + 1 executor.
        assert_eq!(d.calls.len(), 4);
        let exec_prompt = &d.calls[3].1;
        assert!(exec_prompt.contains("## Branch 1"));
        assert!(exec_prompt.contains("OUT[wide-plan-exec#0.0]"));
        assert!(exec_prompt.contains("OUT[wide-plan-exec#0.2]"));
    }
}
