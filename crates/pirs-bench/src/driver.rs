//! The task driver — the skeleton state machine. It owns phase order and the
//! non-negotiable invariant that a task is only `Solved` when the verification
//! [`gate`](crate::gate) returns `Done`. The localization + editing step is
//! injected as an [`Executor`]; that is where the agent (and optionally a
//! strong-model planner/steerer) plugs in. The driver itself is deterministic
//! and cannot be talked past.

use std::collections::BTreeSet;

use crate::baseline::{capture_stable, targets_reproduce};
use crate::gate::Verdict;
use crate::run::{verify, TestRunner, VerifyPlan};
use crate::types::{FailBucket, Outcome, Ring, TestId};

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

/// Run one task to a terminal [`Outcome`]. Bootstrap/discovery happen upstream
/// (they produce the `runner`); this drives baseline → reproduce → fix/verify.
pub fn run_task(
    spec: &TaskSpec,
    runner: &dyn TestRunner,
    executor: &mut dyn Executor,
    max_attempts: u32,
) -> anyhow::Result<Outcome> {
    // Regression scope = targets ∪ keep_green, deduped and stable-ordered.
    let scope: Vec<TestId> = {
        let mut set: BTreeSet<TestId> = spec.targets.iter().cloned().collect();
        set.extend(spec.keep_green.iter().cloned());
        set.into_iter().collect()
    };

    // Baseline over the whole scope, captured stably (guards against flakiness).
    let baseline = match capture_stable(runner, &scope, Ring::Scoped)? {
        Some(b) => b,
        None => return Ok(Outcome::Failed(FailBucket::BaselineUnusable)),
    };

    // Reproduce: every target must be red at baseline, or there is nothing to fix.
    if targets_reproduce(&baseline, &spec.targets).is_err() {
        return Ok(Outcome::Failed(FailBucket::ReproFailed));
    }

    // Fix/verify loop. Only a gate `Done` yields `Solved`.
    let mut last: Option<Verdict> = None;
    for attempt in 1..=max_attempts {
        if !executor.attempt(attempt, last.as_ref())? {
            break; // executor gave up
        }
        let plan = VerifyPlan { targets: &spec.targets, scope: &scope, baseline: &baseline };
        let verdict = verify(runner, &plan, Ring::Scoped)?;
        if verdict.is_done() {
            return Ok(Outcome::Solved);
        }
        last = Some(verdict);
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
        assert_eq!(run_task(&spec, &runner, &mut exec, 3).unwrap(), Outcome::Solved);
    }

    #[test]
    fn never_fixing_exhausts_to_fix_no_flip() {
        let fixed = Cell::new(false);
        let runner = FlagRunner { fixed: &fixed, target: "t1" };
        let mut exec = FlagExecutor { fixed: &fixed, fix_on: 0 };
        let spec = TaskSpec { targets: ids(&["t1"]), keep_green: vec![] };
        assert_eq!(
            run_task(&spec, &runner, &mut exec, 3).unwrap(),
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
            run_task(&spec, &Flaky { n: Cell::new(0) }, &mut NoExec, 3).unwrap(),
            Outcome::Failed(FailBucket::BaselineUnusable)
        );
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
            run_task(&spec, &runner, &mut NoExec, 3).unwrap(),
            Outcome::Failed(FailBucket::ReproFailed)
        );
    }
}
