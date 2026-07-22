//! End-to-end instance runner — the glue that turns the primitives into an
//! actual harness. Given a task instance it runs the full pipeline:
//!
//!   discover runner → bootstrap env → build runner → (cached) baseline →
//!   reproduce → fix/verify loop → terminal [`Outcome`]
//!
//! Every stage that can fail maps to a typed [`FailBucket`] so the aggregate
//! [`Attribution`](crate::report::Attribution) histogram shows exactly where a
//! run was lost — detection, environment, reproduction, or the fix itself. The
//! localization + editing step stays injected as an [`Executor`]; this function
//! owns only the deterministic scaffolding around it.

use std::path::PathBuf;

use crate::bootstrap::{bootstrap, Bootstrap};
use crate::cache::BaselineCache;
use crate::command::CommandRunner;
use crate::detect::DetectorHost;
use crate::driver::{run_task, run_task_cached, Executor, TaskSpec};
use crate::git::GitWorkspace;
use crate::probe::probe;
use crate::run::TestRunner;
use crate::timing::Timings;
use crate::types::{FailBucket, Outcome, RunnerSpec, TestId};

/// A single benchmark instance to attempt.
pub struct Instance {
    pub repo_root: PathBuf,
    pub targets: Vec<TestId>,
    /// Tests that must stay green (regression scope). May be empty.
    pub keep_green: Vec<TestId>,
    /// Checkout commit SHA. When set, baseline capture is cached by it.
    pub base_sha: Option<String>,
}

/// The result of attempting one instance: the terminal [`Outcome`] and, when the
/// fix was accepted and a workspace was supplied, the unified diff of the fix.
#[derive(Debug, Clone)]
pub struct InstanceReport {
    pub outcome: Outcome,
    /// The extracted patch — `Some` only on an accepted outcome with a workspace.
    pub patch: Option<String>,
    /// Per-phase wall-clock: discover, bootstrap, baseline, fix, verify, patch.
    pub timings: Timings,
    /// True when no static detector confirmed a runner and `undetected_fallback`
    /// supplied one instead. That runner's verdicts are NOT independently
    /// re-verified by the harness the way every other [`TestRunner`]'s are —
    /// callers/traces MUST surface this so a reader never mistakes a
    /// self-reported outcome for a harness-confirmed one.
    pub used_undetected_fallback: bool,
}

/// Run one instance to an [`InstanceReport`]. `cache` is threaded through so a
/// batch of instances at the same checkout reuses baselines. When `workspace` is
/// `Some`, an accepted outcome yields the fix as a patch and any other outcome
/// rolls the tree back to pristine — so a failed attempt never leaves partial
/// edits on disk.
///
/// `undetected_fallback`, when given, is invoked ONLY if no static detector
/// confirms any runner at all — it must produce a substitute [`TestRunner`]
/// (e.g. one backed by an agent's self-report) rather than failing the
/// instance outright with `RunnerUndetected`. Bootstrap is skipped for a
/// fallback runner (there is no static [`RunnerSpec`] to install/probe); the
/// fallback owns making the environment usable itself, if needed at all.
pub fn run_instance(
    inst: &Instance,
    host: &DetectorHost,
    cache: &mut BaselineCache,
    executor: &mut dyn Executor,
    max_attempts: u32,
    workspace: Option<&GitWorkspace>,
    undetected_fallback: Option<&dyn Fn() -> Box<dyn TestRunner>>,
) -> anyhow::Result<InstanceReport> {
    let mut timings = Timings::new();
    let bail = |outcome, timings| {
        Ok(InstanceReport {
            outcome,
            patch: None,
            timings,
            used_undetected_fallback: false,
        })
    };

    // 1. Discover every runner candidate whose probe confirms (read-only, no
    //    installs yet), in trust order. No confirmed candidate → we can't get a
    //    pass/fail signal at all. (No edits yet, so no rollback needed.)
    let confirmed: Vec<RunnerSpec> = timings.time("discover", || -> anyhow::Result<_> {
        let mut out = Vec::new();
        for candidate in host.detect(&inst.repo_root) {
            if probe(&candidate, &inst.repo_root)?.confirmed {
                out.push(candidate);
            }
        }
        Ok(out)
    })?;

    // 2. Bootstrap every confirmed candidate that can install. We keep the full
    //    ready list (not just the first) so a later ReproFailed / BaselineUnusable
    //    can fall through to the next runner — "detected-wrong" is not the same
    //    as "undetected", and quitting on the first green-at-baseline wrapper
    //    wastes the instance (observed: sympy specialized runner vs pytest).
    let mut ready_specs: Vec<RunnerSpec> = Vec::new();
    let mut last_hint = String::new();
    let had_confirmed = !confirmed.is_empty();
    if had_confirmed {
        timings.time("bootstrap", || -> anyhow::Result<()> {
            for candidate in confirmed {
                match bootstrap(&candidate, &inst.repo_root)? {
                    Bootstrap::Ready(_) => ready_specs.push(candidate),
                    Bootstrap::Failed(hint) => last_hint = hint,
                }
            }
            Ok(())
        })?;
    }

    let task = TaskSpec {
        targets: inst.targets.clone(),
        keep_green: inst.keep_green.clone(),
    };

    /// Outcomes that mean "this runner cannot reproduce / baseline" — safe to
    /// try the next candidate. Anything else (Solved, FixNoFlip, agent ran) is
    /// terminal for the multi-candidate ladder.
    fn runner_unusable(o: &Outcome) -> bool {
        matches!(
            o,
            Outcome::Failed(FailBucket::ReproFailed)
                | Outcome::Failed(FailBucket::BaselineUnusable)
        )
    }

    let mut used_undetected_fallback = false;
    let mut outcome: Option<Outcome> = None;

    if ready_specs.is_empty() && !had_confirmed {
        // No static detector confirmed anything.
        match undetected_fallback {
            Some(make_fallback) => {
                tracing::warn!(
                    "no runner confirmed; falling back to an agent-discovered runner \
                     (self-reported, not independently verified)"
                );
                used_undetected_fallback = true;
                let runner = make_fallback();
                outcome = Some(match &inst.base_sha {
                    Some(sha) => run_task_cached(
                        &task,
                        &*runner,
                        executor,
                        max_attempts,
                        cache,
                        sha,
                        &mut timings,
                    )?,
                    None => run_task(&task, &*runner, executor, max_attempts, &mut timings)?,
                });
            }
            None => {
                tracing::warn!("no runner confirmed");
                return bail(Outcome::Failed(FailBucket::RunnerUndetected), timings);
            }
        }
    } else if ready_specs.is_empty() {
        tracing::warn!("environment setup failed for every candidate runner: {last_hint}");
        return bail(Outcome::Failed(FailBucket::EnvSetup), timings);
    } else {
        // 3. Try ready runners in trust order. First success path (or non-repro
        //    failure after the agent could start) wins. On ReproFailed /
        //    BaselineUnusable, advance — and use uncached baseline for retries
        //    so a prior runner's green cache cannot poison the next.
        for (i, spec) in ready_specs.iter().enumerate() {
            let runner: Box<dyn TestRunner> =
                Box::new(CommandRunner::new(spec.clone(), inst.repo_root.clone()));
            let o = if i == 0 {
                match &inst.base_sha {
                    Some(sha) => run_task_cached(
                        &task,
                        &*runner,
                        executor,
                        max_attempts,
                        cache,
                        sha,
                        &mut timings,
                    )?,
                    None => run_task(&task, &*runner, executor, max_attempts, &mut timings)?,
                }
            } else {
                tracing::warn!(
                    framework = %spec.framework,
                    "prior runner failed reproduce/baseline; trying next candidate"
                );
                // Fresh baseline for this runner — ignore SHA cache from peers.
                run_task(&task, &*runner, executor, max_attempts, &mut timings)?
            };
            if !runner_unusable(&o) {
                outcome = Some(o);
                break;
            }
            outcome = Some(o);
        }

        // Still stuck on repro with only static runners → optional agent fallback.
        if outcome.as_ref().is_some_and(runner_unusable) {
            if let Some(make_fallback) = undetected_fallback {
                tracing::warn!(
                    "all static runners failed reproduce/baseline; agent-discovery fallback"
                );
                used_undetected_fallback = true;
                let runner = make_fallback();
                outcome = Some(run_task(
                    &task,
                    &*runner,
                    executor,
                    max_attempts,
                    &mut timings,
                )?);
            }
        }
    }

    let outcome = outcome.expect("outcome set on every non-bail path");

    // 4. Keep the fix (as a patch) or roll back to pristine.
    let patch = match workspace {
        Some(ws) if outcome.is_accepted() => Some(timings.time("patch", || ws.diff())?),
        Some(ws) => {
            timings.time("rollback", || ws.reset())?; // discard partial/failed edits
            None
        }
        None => None,
    };

    Ok(InstanceReport {
        outcome,
        patch,
        timings,
        used_undetected_fallback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::Verdict;

    struct NeverExecutor;
    impl Executor for NeverExecutor {
        fn attempt(&mut self, _a: u32, _l: Option<&Verdict>) -> anyhow::Result<bool> {
            panic!("executor must not run when no runner is detected")
        }
    }

    #[test]
    fn undetected_runner_short_circuits_before_executor() {
        // An empty dir → no detector confirms → RunnerUndetected, and the
        // executor is never consulted.
        let dir = tempfile::tempdir().unwrap();
        let host = DetectorHost::with_bundled().unwrap();
        let mut cache = BaselineCache::in_memory();
        let inst = Instance {
            repo_root: dir.path().to_path_buf(),
            targets: vec!["t1".into()],
            keep_green: vec![],
            base_sha: None,
        };
        let report =
            run_instance(&inst, &host, &mut cache, &mut NeverExecutor, 3, None, None).unwrap();
        assert_eq!(
            report.outcome,
            Outcome::Failed(FailBucket::RunnerUndetected)
        );
        assert!(report.patch.is_none());
        assert!(!report.used_undetected_fallback);
    }

    /// A fixed-outcome runner standing in for a real agent-discovery runner —
    /// this test only proves harness.rs's wiring (fallback invoked, bootstrap
    /// skipped, `used_undetected_fallback` set, outcome flows through the
    /// normal driver), not the agent itself.
    /// Red for its first `flips_after` calls (covering the baseline's own
    /// stability check, which runs the fallback runner twice), green from
    /// then on — simulating "the fix landed" without a real subprocess.
    struct FlippingRunner {
        calls: std::cell::Cell<u32>,
        flips_after: u32,
    }
    impl TestRunner for FlippingRunner {
        fn run(
            &self,
            ids: &[TestId],
            _ring: crate::types::Ring,
        ) -> anyhow::Result<crate::types::Snapshot> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            let outcome = if n < self.flips_after {
                crate::types::TestOutcome::Fail
            } else {
                crate::types::TestOutcome::Pass
            };
            Ok(crate::types::Snapshot::from_pairs(
                ids.iter().map(|id| (id.clone(), outcome)),
            ))
        }
    }

    struct OneShotExecutor {
        ran: std::cell::Cell<bool>,
    }
    impl Executor for OneShotExecutor {
        fn attempt(&mut self, _a: u32, _l: Option<&Verdict>) -> anyhow::Result<bool> {
            self.ran.set(true);
            Ok(true) // claim a change was made, so the driver verifies
        }
    }

    #[test]
    fn undetected_fallback_bypasses_bootstrap_and_is_flagged_in_the_report() {
        // Baseline (pre-fix) reports every target red via the fallback runner;
        // after one "fix" attempt the same fallback reports every target green
        // — so the instance is accepted purely through the fallback runner,
        // with bootstrap/CommandRunner never in the picture.
        let dir = tempfile::tempdir().unwrap();
        let host = DetectorHost::with_bundled().unwrap();
        let mut cache = BaselineCache::in_memory();
        let inst = Instance {
            repo_root: dir.path().to_path_buf(),
            targets: vec!["t1".into()],
            keep_green: vec![],
            base_sha: None,
        };
        let mut executor = OneShotExecutor {
            ran: std::cell::Cell::new(false),
        };
        // Baseline capture calls .run() twice to confirm stability before the
        // fix loop ever starts — flip on the 3rd call (the post-fix verify).
        let fallback_fn = || -> Box<dyn TestRunner> {
            Box::new(FlippingRunner {
                calls: std::cell::Cell::new(0),
                flips_after: 2,
            })
        };
        let report = run_instance(
            &inst,
            &host,
            &mut cache,
            &mut executor,
            3,
            None,
            Some(&fallback_fn),
        )
        .unwrap();
        assert!(report.used_undetected_fallback);
        assert!(
            executor.ran.get(),
            "executor must run under the fallback path too"
        );
        assert!(report.outcome.is_accepted(), "{:?}", report.outcome);
    }
}
