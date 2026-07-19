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
}

/// A named, ordered sequence of phases.
#[derive(Debug, Clone)]
pub struct Strategy {
    pub name: String,
    pub phases: Vec<Phase>,
    /// When true, the (single) phase's context is reused across attempts so the
    /// agent accumulates memory — the "monolithic" baseline. When false, every
    /// phase starts from a clean context each attempt (the plan/execute split,
    /// whose whole point is a fresh executor seeded with only the plan).
    pub persist_across_attempts: bool,
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
    fn run_phase(
        &mut self,
        phase_id: &str,
        system: &str,
        prompt: &str,
        scope: ToolScope,
        fresh: bool,
    ) -> anyhow::Result<String>;
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
    let mut prev = String::new();
    for (i, phase) in strategy.phases.iter().enumerate() {
        let phase_id = format!("{}#{i}", strategy.name);
        let prompt = render(&phase.prompt, task, &prev);
        // Persistent strategies continue their context across attempts; split
        // strategies always start each phase clean.
        let fresh = !strategy.persist_across_attempts;
        prev = driver.run_phase(&phase_id, &phase.system, &prompt, phase.scope, fresh)?;
    }
    Ok(())
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

impl Strategy {
    /// One self-correcting loop in a persistent context. The baseline.
    pub fn monolithic() -> Self {
        Strategy {
            name: "monolithic".into(),
            persist_across_attempts: true,
            phases: vec![Phase {
                system: MONO_SYSTEM.into(),
                prompt: MONO_PROMPT.into(),
                scope: ToolScope::Full,
            }],
        }
    }

    /// Read-only planner → fresh executor seeded with only the plan.
    pub fn plan_exec() -> Self {
        Strategy {
            name: "plan-exec".into(),
            persist_across_attempts: false,
            phases: vec![
                Phase {
                    system: PLAN_SYSTEM.into(),
                    prompt: PLAN_PROMPT.into(),
                    scope: ToolScope::ReadOnly,
                },
                Phase {
                    system: EXEC_SYSTEM.into(),
                    prompt: EXEC_PROMPT.into(),
                    scope: ToolScope::Full,
                },
            ],
        }
    }

    /// Planner → critic gate → fresh executor.
    pub fn plan_critic_exec() -> Self {
        Strategy {
            name: "plan-critic-exec".into(),
            persist_across_attempts: false,
            phases: vec![
                Phase {
                    system: PLAN_SYSTEM.into(),
                    prompt: PLAN_PROMPT.into(),
                    scope: ToolScope::ReadOnly,
                },
                Phase {
                    system: CRITIC_SYSTEM.into(),
                    prompt: CRITIC_PROMPT.into(),
                    scope: ToolScope::ReadOnly,
                },
                Phase {
                    system: EXEC_SYSTEM.into(),
                    prompt: EXEC_PROMPT.into(),
                    scope: ToolScope::Full,
                },
            ],
        }
    }

    /// Look up a built-in strategy by name.
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "monolithic" => Some(Self::monolithic()),
            "plan-exec" => Some(Self::plan_exec()),
            "plan-critic-exec" => Some(Self::plan_critic_exec()),
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
        calls: Vec<(String, ToolScope, bool, String)>,
    }
    impl PhaseDriver for RecordingDriver {
        fn run_phase(
            &mut self,
            phase_id: &str,
            _system: &str,
            prompt: &str,
            scope: ToolScope,
            fresh: bool,
        ) -> anyhow::Result<String> {
            self.calls
                .push((phase_id.to_string(), scope, fresh, prompt.to_string()));
            // Return a phase-specific marker to trace it into the next {prev}.
            Ok(format!("OUTPUT_OF[{phase_id}]"))
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
        let (id, scope, fresh, prompt) = &d.calls[0];
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
        assert_eq!(Strategy::builtin("plan-exec").unwrap().phases.len(), 2);
        assert!(Strategy::builtin("nope").is_none());
    }
}
