//! Loop strategies — how an agent's work is *structured* into phases.
//!
//! A [`Strategy`] is plain data: an ordered list of [`Phase`]s, each a system
//! prompt, a prompt template, and a [`ToolScope`]. The engine ([`run_strategy`])
//! walks the phases, renders each template against the [`Task`] (issue, targets,
//! the prior phase's output, the last verdict), and drives it through a
//! [`PhaseDriver`] — the actual agent loop, supplied by the caller.
//!
//! This split is deliberate:
//! - The **policy** (which phases, what prompts, which tools) is data. The three
//!   built-ins ([`Strategy::monolithic`], [`Strategy::plan_exec`],
//!   [`Strategy::plan_critic_exec`]) are just values, and a user can author their
//!   own — including from a script — without touching Rust.
//! - The **engine** (running a sub-loop, managing context, token/behaviour
//!   accounting) is a [`PhaseDriver`] the host implements once.
//!
//! Nothing here is benchmark-specific; the bench harness is one consumer.

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

// ---- Built-in strategies ---------------------------------------------------

/// System prompt for a single self-correcting fix loop, and for the executor
/// phase of a split strategy.
const MONO_SYSTEM: &str = "\
You are fixing a bug in a real code repository so that specific failing tests pass.

Rules:
- Make the SMALLEST change that makes the failing tests pass. Do not refactor.
- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.
- Use the read and code-graph tools to locate the real cause before editing.
- You may run the project's tests to check your work.
- When the target tests pass and you have not broken others, stop.";

/// Planner: investigate and emit a self-contained plan, no edits.
const PLAN_SYSTEM: &str = "\
You are a senior engineer producing a fix plan for a bug in a real repository.

- Investigate with the read / search / code-graph tools to find the true root cause.
- Do NOT edit any files. Output only a plan.
- The plan must be SELF-CONTAINED: a fresh engineer with no memory of your
  investigation will execute it. Name the exact file(s) and function(s), state
  the precise change, and keep it minimal.
- Never propose editing, deleting, or weakening tests. The fix is in source.";

/// Executor: carry out an already-approved plan.
const EXEC_SYSTEM: &str = "\
You are fixing a bug in a real repository by executing an APPROVED plan.

- Make the edits the plan specifies, and nothing more. Keep the change minimal.
- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.
- You may run the project's tests to check your work.
- Follow the plan; if a step is clearly wrong, make the smallest correct fix.
- When the target tests pass and you have not broken others, stop.";

/// Critic: vet (and if needed correct) a plan before execution.
const CRITIC_SYSTEM: &str = "\
You are reviewing a proposed fix plan before it is executed.

Check it against the issue: does it target the real root cause, is it minimal,
does it avoid touching tests, will it make the failing tests pass? You may read
the code to verify. If the plan is sound, output it unchanged. If it is flawed,
output a corrected, self-contained plan. Output ONLY the final plan to execute.";

const MONO_PROMPT: &str = "\
{verdict}Fix the following issue in this repository.

## Issue
{issue}

## Tests that must pass after your fix
{targets}

Locate the cause, make the minimal source change, and verify.";

const PLAN_PROMPT: &str = "\
{verdict}Investigate and produce a self-contained fix plan.

## Issue
{issue}

## Tests that must pass after the fix
{targets}

Output ONLY the numbered plan (files, functions, exact changes). Do not edit.";

const EXEC_PROMPT: &str = "\
Execute this approved fix plan in the repository.

## Issue
{issue}

## Tests that must pass
{targets}

## Approved plan
{prev}

Make the edits now and verify.";

const CRITIC_PROMPT: &str = "\
## Issue
{issue}

## Tests that must pass
{targets}

## Proposed plan
{prev}

Review and output the final plan to execute.";

/// A phase on the run's default model.
fn phase(system: &str, prompt: &str, scope: ToolScope) -> Phase {
    Phase {
        system: system.into(),
        prompt: prompt.into(),
        scope,
        model: None,
    }
}

/// A single-phase step on the default model.
fn solo(system: &str, prompt: &str, scope: ToolScope) -> Step {
    Step::Solo(phase(system, prompt, scope))
}

/// Prompt angles for the wide (parallel) planner: each branch investigates from a
/// different starting point, so the branches don't collapse onto one hypothesis.
const WIDE_ANGLES: &[&str] = &[
    "Focus on the failing assertion itself: what value is produced vs expected, and \
     which function computes it.",
    "Focus on the most recently changed or most complex code path touched by the \
     failing test.",
    "Focus on boundary/edge handling (empty, zero, off-by-one, sign) in the code \
     under test.",
];

impl Strategy {
    /// One self-correcting loop in a persistent context. The baseline.
    pub fn monolithic() -> Self {
        Strategy {
            name: "monolithic".into(),
            persist_across_attempts: true,
            steps: vec![solo(MONO_SYSTEM, MONO_PROMPT, ToolScope::Full)],
        }
    }

    /// Read-only planner → fresh executor seeded with only the plan.
    pub fn plan_exec() -> Self {
        Strategy {
            name: "plan-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                solo(PLAN_SYSTEM, PLAN_PROMPT, ToolScope::ReadOnly),
                solo(EXEC_SYSTEM, EXEC_PROMPT, ToolScope::Full),
            ],
        }
    }

    /// Planner → critic gate → fresh executor.
    pub fn plan_critic_exec() -> Self {
        Strategy {
            name: "plan-critic-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                solo(PLAN_SYSTEM, PLAN_PROMPT, ToolScope::ReadOnly),
                solo(CRITIC_SYSTEM, CRITIC_PROMPT, ToolScope::ReadOnly),
                solo(EXEC_SYSTEM, EXEC_PROMPT, ToolScope::Full),
            ],
        }
    }

    /// Plan → **Oracle critic on a different model** → execute. The Amp pattern:
    /// the second opinion comes from a stronger/other model than the executor.
    /// `oracle_model` runs only the critic phase; the rest use the run default.
    pub fn plan_oracle_exec(oracle_model: &str) -> Self {
        let mut critic = phase(CRITIC_SYSTEM, CRITIC_PROMPT, ToolScope::ReadOnly);
        critic.model = Some(oracle_model.to_string());
        Strategy {
            name: "plan-oracle-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                solo(PLAN_SYSTEM, PLAN_PROMPT, ToolScope::ReadOnly),
                Step::Solo(critic),
                solo(EXEC_SYSTEM, EXEC_PROMPT, ToolScope::Full),
            ],
        }
    }

    /// `n` planners explore **in parallel** from different angles, their findings
    /// are merged, then a fresh executor acts on the combined plan. The SoulForge
    /// dispatch pattern: concurrent read-only research, then a single edit.
    pub fn wide_plan_exec(n: usize) -> Self {
        let n = n.clamp(2, WIDE_ANGLES.len());
        let branches = (0..n)
            .map(|i| {
                let prompt = format!("{}\n\n{}", WIDE_ANGLES[i], PLAN_PROMPT);
                phase(PLAN_SYSTEM, &prompt, ToolScope::ReadOnly)
            })
            .collect();
        Strategy {
            name: "wide-plan-exec".into(),
            persist_across_attempts: false,
            steps: vec![
                Step::Fan {
                    branches,
                    join: Join::Concat,
                },
                solo(EXEC_SYSTEM, EXEC_PROMPT, ToolScope::Full),
            ],
        }
    }

    /// Names of the built-in strategies resolvable by [`Self::builtin`].
    /// (`plan-oracle-exec` is excluded — it needs an oracle model argument and
    /// cannot be constructed from a bare name.)
    pub fn builtin_names() -> &'static [&'static str] {
        &[
            "monolithic",
            "plan-exec",
            "plan-critic-exec",
            "wide-plan-exec",
        ]
    }

    /// Look up a built-in strategy by name.
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "monolithic" => Some(Self::monolithic()),
            "plan-exec" => Some(Self::plan_exec()),
            "plan-critic-exec" => Some(Self::plan_critic_exec()),
            "wide-plan-exec" => Some(Self::wide_plan_exec(3)),
            _ => None,
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
        run_strategy(&Strategy::monolithic(), &mut d, &task()).unwrap();
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
        run_strategy(&Strategy::plan_exec(), &mut d, &task()).unwrap();
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
        run_strategy(&Strategy::plan_critic_exec(), &mut d, &task()).unwrap();
        assert_eq!(d.calls.len(), 3);
        assert_eq!(d.calls[1].1, ToolScope::ReadOnly); // critic reads, doesn't edit
        assert!(d.calls[1].3.contains("OUTPUT_OF[plan-critic-exec#0]")); // sees plan
        assert!(d.calls[2].3.contains("OUTPUT_OF[plan-critic-exec#1]")); // exec sees vetted plan
        assert_eq!(d.calls[2].1, ToolScope::Full);
    }

    #[test]
    fn builtin_lookup_matches_names_and_rejects_unknown() {
        assert_eq!(Strategy::builtin("plan-exec").unwrap().steps.len(), 2);
        assert!(Strategy::builtin("nope").is_none());
    }

    #[test]
    fn oracle_runs_the_critic_phase_on_a_different_model() {
        let mut d = RecordingDriver::default();
        run_strategy(&Strategy::plan_oracle_exec("strong-model"), &mut d, &task()).unwrap();
        assert_eq!(d.calls.len(), 3);
        // Plan and exec use the default (None); only the critic overrides.
        assert_eq!(d.calls[0].4, None);
        assert_eq!(d.calls[1].4, Some("strong-model".to_string()));
        assert_eq!(d.calls[2].4, None);
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
        run_strategy(&Strategy::wide_plan_exec(3), &mut d, &task()).unwrap();
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
        run_strategy(&Strategy::wide_plan_exec(2), &mut d, &task()).unwrap();
        // One fan-out step of width 2 went through run_parallel, not run_phase.
        assert_eq!(d.parallel_widths, vec![2]);
    }

    #[test]
    fn fan_out_tolerates_partial_branch_failure() {
        let mut d = RecordingDriver {
            fail: vec!["wide-plan-exec#0.1".into()],
            ..Default::default()
        };
        run_strategy(&Strategy::wide_plan_exec(3), &mut d, &task()).unwrap();
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
        let err = run_strategy(&Strategy::wide_plan_exec(2), &mut d, &task()).unwrap_err();
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
        run_strategy_async(&Strategy::plan_exec(), &mut d, &task())
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
        run_strategy_async(&Strategy::wide_plan_exec(3), &mut d, &task())
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
