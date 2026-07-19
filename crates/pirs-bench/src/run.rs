//! The runner seam and the verification orchestrator.
//!
//! [`TestRunner`] abstracts "run these tests, report their outcomes" so the
//! orchestrator is exercised without spawning subprocesses; the real
//! implementation (JUnit-emitting subprocess) plugs in behind the same trait.

use crate::gate::{self, Verdict};
use crate::types::{Ring, Snapshot, TestId};

/// Runs a set of tests and reports per-test outcomes for one [`Ring`].
pub trait TestRunner {
    fn run(&self, ids: &[TestId], ring: Ring) -> anyhow::Result<Snapshot>;
}

/// What a verification pass needs: the tests that must flip, the regression
/// scope for the ring, and the captured baseline to diff against.
pub struct VerifyPlan<'a> {
    pub targets: &'a [TestId],
    pub scope: &'a [TestId],
    pub baseline: &'a Snapshot,
}

/// Run the differential gate for one ring. The confirmation run (`post2`) is
/// **only** performed when the fix has provisionally landed — a still-red target
/// or a regression short-circuits before paying for a second subprocess.
pub fn verify(runner: &dyn TestRunner, plan: &VerifyPlan, ring: Ring) -> anyhow::Result<Verdict> {
    let post = runner.run(plan.scope, ring)?;
    let prov = gate::provisional(plan.targets, plan.scope, plan.baseline, &post);
    if !prov.is_done() {
        return Ok(prov);
    }
    // Provisionally green — confirm the flips on a fresh targets-only run.
    let post2 = runner.run(plan.targets, Ring::Inner)?;
    Ok(gate::confirm_flips(plan.targets, &post2).unwrap_or(Verdict::Done))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome::*;
    use std::cell::RefCell;

    /// A runner that replays scripted snapshots and records how it was called,
    /// so tests can assert the confirmation run happens only when warranted.
    struct MockRunner {
        scripted: Vec<Snapshot>,
        calls: RefCell<Vec<Vec<TestId>>>,
        idx: RefCell<usize>,
    }
    impl MockRunner {
        fn new(scripted: Vec<Snapshot>) -> Self {
            MockRunner { scripted, calls: RefCell::new(vec![]), idx: RefCell::new(0) }
        }
        fn call_count(&self) -> usize {
            self.calls.borrow().len()
        }
    }
    impl TestRunner for MockRunner {
        fn run(&self, ids: &[TestId], _ring: Ring) -> anyhow::Result<Snapshot> {
            self.calls.borrow_mut().push(ids.to_vec());
            let mut i = self.idx.borrow_mut();
            let snap = self.scripted[*i].clone();
            *i += 1;
            Ok(snap)
        }
    }

    fn ids(v: &[&str]) -> Vec<TestId> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn done_requires_two_runs() {
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let runner = MockRunner::new(vec![
            Snapshot::from_pairs([("t1", Pass)]), // post
            Snapshot::from_pairs([("t1", Pass)]), // post2 (confirmation)
        ]);
        let plan = VerifyPlan { targets: &targets, scope: &targets, baseline: &base };
        assert_eq!(verify(&runner, &plan, Ring::Inner).unwrap(), Verdict::Done);
        assert_eq!(runner.call_count(), 2, "confirmation run must happen");
    }

    #[test]
    fn still_red_skips_confirmation_run() {
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let runner = MockRunner::new(vec![
            Snapshot::from_pairs([("t1", Fail)]), // post: not flipped
        ]);
        let plan = VerifyPlan { targets: &targets, scope: &targets, baseline: &base };
        assert_eq!(verify(&runner, &plan, Ring::Inner).unwrap(), Verdict::NotYet("t1".into()));
        // The second (subprocess-costly) run must be skipped.
        assert_eq!(runner.call_count(), 1, "no confirmation run when unfixed");
    }

    #[test]
    fn regression_skips_confirmation_run() {
        let targets = ids(&["t1"]);
        let scope = ids(&["t1", "n"]);
        let base = Snapshot::from_pairs([("t1", Fail), ("n", Pass)]);
        let runner = MockRunner::new(vec![
            Snapshot::from_pairs([("t1", Pass), ("n", Fail)]), // post: regressed n
        ]);
        let plan = VerifyPlan { targets: &targets, scope: &scope, baseline: &base };
        assert_eq!(verify(&runner, &plan, Ring::Scoped).unwrap(), Verdict::Regressed("n".into()));
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn flip_that_does_not_hold_is_flaky() {
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let runner = MockRunner::new(vec![
            Snapshot::from_pairs([("t1", Pass)]), // post: flipped
            Snapshot::from_pairs([("t1", Fail)]), // post2: didn't hold
        ]);
        let plan = VerifyPlan { targets: &targets, scope: &targets, baseline: &base };
        assert_eq!(verify(&runner, &plan, Ring::Inner).unwrap(), Verdict::Flaky("t1".into()));
        assert_eq!(runner.call_count(), 2);
    }
}
