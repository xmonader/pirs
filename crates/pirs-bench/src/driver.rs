//! The task driver — the skeleton state machine. It owns phase order and the
//! non-negotiable invariant that a task is only `Solved` when the verification
//! [`gate`](crate::gate) returns `Done`. The localization + editing step is
//! injected as an [`Executor`]; that is where the agent (and optionally a
//! strong-model planner/steerer) plugs in. The driver itself is deterministic
//! and cannot be talked past.

use std::collections::BTreeSet;

use crate::baseline::{capture_stable, capture_stable_cached, targets_reproduce};
use crate::cache::BaselineCache;
use crate::gate::Verdict;
use crate::run::{verify, TestRunner, VerifyPlan};
use crate::timing::Timings;
use crate::types::{FailBucket, Outcome, Ring, Snapshot, TestId};

/// The inputs that define a task.
pub struct TaskSpec {
    pub targets: Vec<TestId>,
    /// Tests that must stay green (regression scope). May be empty.
    pub keep_green: Vec<TestId>,
}

/// The pluggable fix step: localize + edit. Returns `true` if it produced a
/// change worth verifying, `false` to stop trying. `last` is the previous
/// attempt's verdict, so the executor can steer on the exact failure.
pub trait Executor {
    fn attempt(&mut self, attempt: u32, last: Option<&Verdict>) -> anyhow::Result<bool>;
}

/// Regression scope = targets ∪ keep_green, deduped and stable-ordered.
fn compute_scope(spec: &TaskSpec) -> Vec<TestId> {
    let mut set: BTreeSet<TestId> = spec.targets.iter().cloned().collect();
    set.extend(spec.keep_green.iter().cloned());
    set.into_iter().collect()
}

/// Run one task to a terminal [`Outcome`]. Bootstrap/discovery happen upstream
/// (they produce the `runner`); this drives baseline → reproduce → fix/verify.
pub fn run_task(
    spec: &TaskSpec,
    runner: &dyn TestRunner,
    executor: &mut dyn Executor,
    max_attempts: u32,
    timings: &mut Timings,
) -> anyhow::Result<Outcome> {
    let scope = compute_scope(spec);
    let baseline = match timings.time("baseline", || capture_stable(runner, &scope, Ring::Scoped))? {
        Some(b) => b,
        None => return Ok(Outcome::Failed(FailBucket::BaselineUnusable)),
    };
    drive(spec, &scope, baseline, runner, executor, max_attempts, timings)
}

/// Like [`run_task`] but captures the baseline through a SHA-keyed
/// [`BaselineCache`], so repeated attempts/tasks at the same checkout skip the
/// double-run. Falls back to a fresh capture for uncached tests.
pub fn run_task_cached(
    spec: &TaskSpec,
    runner: &dyn TestRunner,
    executor: &mut dyn Executor,
    max_attempts: u32,
    cache: &mut BaselineCache,
    base_sha: &str,
    timings: &mut Timings,
) -> anyhow::Result<Outcome> {
    let scope = compute_scope(spec);
    let baseline = match timings
        .time("baseline", || capture_stable_cached(runner, &scope, Ring::Scoped, cache, base_sha))?
    {
        Some(b) => b,
        None => return Ok(Outcome::Failed(FailBucket::BaselineUnusable)),
    };
    drive(spec, &scope, baseline, runner, executor, max_attempts, timings)
}

/// The reproduce gate + fix/verify loop over an already-captured baseline. Shared
/// by the cached and uncached entry points.
fn drive(
    spec: &TaskSpec,
    scope: &[TestId],
    baseline: Snapshot,
    runner: &dyn TestRunner,
    executor: &mut dyn Executor,
    max_attempts: u32,
    timings: &mut Timings,
) -> anyhow::Result<Outcome> {
    // Reproduce: every target must be red at baseline, or there is nothing to fix.
    if targets_reproduce(&baseline, &spec.targets).is_err() {
        return Ok(Outcome::Failed(FailBucket::ReproFailed));
    }

    // Concentric rings (cost control). Each refinement attempt verifies only the
    // Inner ring (the targets) — the cheap signal. The full regression ring runs
    // at most once *per flip*, only after the Inner ring goes green, so a failing
    // attempt never pays for the whole keep-green suite. With no keep_green the
    // two rings coincide and the second pass is skipped.
    let has_regression_ring = scope.len() > spec.targets.len();

    let mut last: Option<Verdict> = None;
    for attempt in 1..=max_attempts {
        // The fix step (the agent) — usually the dominant cost.
        if !timings.time("fix", || executor.attempt(attempt, last.as_ref()))? {
            break; // executor gave up
        }
        let inner = VerifyPlan { targets: &spec.targets, scope: &spec.targets, baseline: &baseline };
        let verdict = timings.time("verify", || verify(runner, &inner, Ring::Inner))?;
        if !verdict.is_done() {
            last = Some(verdict);
            continue;
        }
        if !has_regression_ring {
            return Ok(Outcome::Solved);
        }
        // Targets flipped — now (and only now) pay for the regression ring.
        let scoped = VerifyPlan { targets: &spec.targets, scope, baseline: &baseline };
        let scoped_verdict = timings.time("verify", || verify(runner, &scoped, Ring::Scoped))?;
        if scoped_verdict.is_done() {
            return Ok(Outcome::Solved);
        }
        last = Some(scoped_verdict); // e.g. the fix regressed a keep-green test
    }

    // Exhausted (or the executor stopped). Attribute the last verdict's bucket,
    // defaulting to a no-flip if the executor never produced a candidate.
    let bucket = last
        .as_ref()
        .and_then(Verdict::fail_bucket)
        .unwrap_or(FailBucket::FixNoFlip);
    Ok(Outcome::Failed(bucket))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Snapshot, TestOutcome::*};
    use std::cell::Cell;

    fn ids(v: &[&str]) -> Vec<TestId> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A runner whose target outcome is governed by a shared `fixed` flag, so a
    /// mock executor can "repair" the code between runs. Deterministic within a
    /// flag state, so baseline capture is stable.
    struct FlagRunner<'a> {
        fixed: &'a Cell<bool>,
        target: &'a str,
    }
    impl TestRunner for FlagRunner<'_> {
        fn run(&self, ids: &[TestId], _ring: Ring) -> anyhow::Result<Snapshot> {
            let outcome = if self.fixed.get() { Pass } else { Fail };
            Ok(Snapshot::from_pairs(
                ids.iter().map(|id| {
                    let o = if id == self.target { outcome } else { Pass };
                    (id.clone(), o)
                }),
            ))
        }
    }

    /// Flips `fixed` true on the given attempt; never fixes if `fix_on` is 0.
    struct FlagExecutor<'a> {
        fixed: &'a Cell<bool>,
        fix_on: u32,
    }
    impl Executor for FlagExecutor<'_> {
        fn attempt(&mut self, attempt: u32, _last: Option<&Verdict>) -> anyhow::Result<bool> {
            if self.fix_on != 0 && attempt == self.fix_on {
                self.fixed.set(true);
            }
            Ok(true) // always produces a candidate to verify
        }
    }

    #[test]
    fn fix_on_first_attempt_solves() {
        let fixed = Cell::new(false);
        let runner = FlagRunner { fixed: &fixed, target: "t1" };
        let mut exec = FlagExecutor { fixed: &fixed, fix_on: 1 };
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: ids(&["k"]) };
        assert_eq!(run_task(&spec, &runner, &mut exec, 3, &mut Timings::new()).unwrap(), Outcome::Solved);
    }

    #[test]
    fn never_fixing_exhausts_to_fix_no_flip() {
        let fixed = Cell::new(false);
        let runner = FlagRunner { fixed: &fixed, target: "t1" };
        let mut exec = FlagExecutor { fixed: &fixed, fix_on: 0 };
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: vec![] };
        assert_eq!(
            run_task(&spec, &runner, &mut exec, 3, &mut Timings::new()).unwrap(),
            Outcome::Failed(FailBucket::FixNoFlip)
        );
    }

    #[test]
    fn unusable_baseline_aborts_before_fixing() {
        // A runner that alternates outcomes → baseline never stabilizes.
        struct Flaky {
            n: Cell<u8>,
        }
        impl TestRunner for Flaky {
            fn run(&self, ids: &[TestId], _r: Ring) -> anyhow::Result<Snapshot> {
                let v = self.n.get();
                self.n.set(v + 1);
                let o = if v.is_multiple_of(2) { Fail } else { Pass };
                Ok(Snapshot::from_pairs(ids.iter().map(|i| (i.clone(), o))))
            }
        }
        struct NoExec;
        impl Executor for NoExec {
            fn attempt(&mut self, _a: u32, _l: Option<&Verdict>) -> anyhow::Result<bool> {
                panic!("must not reach the fix loop on an unusable baseline")
            }
        }
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: vec![] };
        assert_eq!(
            run_task(&spec, &Flaky { n: Cell::new(0) }, &mut NoExec, 3, &mut Timings::new()).unwrap(),
            Outcome::Failed(FailBucket::BaselineUnusable)
        );
    }

    /// Before the fix: target red, victim green. After: target green but the fix
    /// broke the victim (a keep-green test). Models a regressing fix.
    struct RegressRunner<'a> {
        fixed: &'a Cell<bool>,
        target: &'a str,
        victim: &'a str,
        runs: &'a Cell<u32>,
    }
    impl TestRunner for RegressRunner<'_> {
        fn run(&self, ids: &[TestId], _r: Ring) -> anyhow::Result<Snapshot> {
            self.runs.set(self.runs.get() + 1);
            let fixed = self.fixed.get();
            Ok(Snapshot::from_pairs(ids.iter().map(|id| {
                let o = if id == self.target {
                    if fixed { Pass } else { Fail }
                } else if id == self.victim {
                    if fixed { Fail } else { Pass } // fix breaks the victim
                } else {
                    Pass
                };
                (id.clone(), o)
            })))
        }
    }

    #[test]
    fn fix_that_regresses_keep_green_is_failed() {
        let fixed = Cell::new(false);
        let runs = Cell::new(0);
        let runner = RegressRunner { fixed: &fixed, target: "t1", victim: "k", runs: &runs };
        let mut exec = FlagExecutor { fixed: &fixed, fix_on: 1 };
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: ids(&["k"]) };
        assert_eq!(
            run_task(&spec, &runner, &mut exec, 3, &mut Timings::new()).unwrap(),
            Outcome::Failed(FailBucket::Regressed)
        );
    }

    /// Counts how often the `victim` (keep-green) id is included in a run, so we
    /// can prove the regression ring fires only after the Inner ring flips.
    struct VictimCountRunner<'a> {
        fixed: &'a Cell<bool>,
        target: &'a str,
        victim: &'a str,
        victim_runs: &'a Cell<u32>,
    }
    impl TestRunner for VictimCountRunner<'_> {
        fn run(&self, ids: &[TestId], _r: Ring) -> anyhow::Result<Snapshot> {
            if ids.iter().any(|id| id == self.victim) {
                self.victim_runs.set(self.victim_runs.get() + 1);
            }
            let fixed = self.fixed.get();
            Ok(Snapshot::from_pairs(ids.iter().map(|id| {
                let o = if id == self.target && !fixed { Fail } else { Pass };
                (id.clone(), o)
            })))
        }
    }

    #[test]
    fn regression_ring_runs_only_after_a_flip() {
        // Executor fixes on attempt 3; attempts 1–2 keep the Inner ring red, so
        // the regression (scoped) ring must never run in them.
        let fixed = Cell::new(false);
        let victim_runs = Cell::new(0);
        let runner = VictimCountRunner {
            fixed: &fixed,
            target: "t1",
            victim: "k",
            victim_runs: &victim_runs,
        };
        let mut exec = FlagExecutor { fixed: &fixed, fix_on: 3 };
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: ids(&["k"]) };
        assert_eq!(run_task(&spec, &runner, &mut exec, 3, &mut Timings::new()).unwrap(), Outcome::Solved);
        // Baseline runs the scope (incl. victim) twice; then the victim appears
        // only in the single scoped pass after the flip (post + post2-is-targets).
        // So victim is included in: 2 (baseline) + 1 (scoped post) = 3 runs.
        assert_eq!(victim_runs.get(), 3, "victim ran outside baseline+one scoped pass");
    }

    #[test]
    fn already_green_target_is_repro_failed() {
        let fixed = Cell::new(true); // target already green at baseline
        let runner = FlagRunner { fixed: &fixed, target: "t1" };
        struct NoExec;
        impl Executor for NoExec {
            fn attempt(&mut self, _a: u32, _l: Option<&Verdict>) -> anyhow::Result<bool> {
                panic!("must not fix when reproduction failed")
            }
        }
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: vec![] };
        assert_eq!(
            run_task(&spec, &runner, &mut NoExec, 3, &mut Timings::new()).unwrap(),
            Outcome::Failed(FailBucket::ReproFailed)
        );
    }
}
