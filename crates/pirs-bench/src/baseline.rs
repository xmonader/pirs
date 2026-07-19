//! Baseline capture and the reproduce gate.
//!
//! The whole task is verified *differentially* against a baseline, so the
//! baseline itself must be trustworthy: a flaky test that reads green on one run
//! and red on the next would poison every later comparison. Capture therefore
//! runs the set twice and requires agreement — otherwise the signal is unusable.

use crate::run::TestRunner;
use crate::types::{Ring, Snapshot, TestId};

/// Capture a **stable** baseline over `ids`: run twice and require identical
/// per-test outcomes. Returns `None` (unstable) if any test disagreed between
/// runs — the caller records `BaselineUnusable` rather than trusting noise.
pub fn capture_stable(
    runner: &dyn TestRunner,
    ids: &[TestId],
    ring: Ring,
) -> anyhow::Result<Option<Snapshot>> {
    let first = runner.run(ids, ring)?;
    let second = runner.run(ids, ring)?;
    for id in ids {
        if first.get(id) != second.get(id) {
            return Ok(None);
        }
    }
    let mut snap = first;
    snap.runs = 2;
    Ok(Some(snap))
}

/// The reproduce gate: every target must be red (Fail/Errored) at baseline.
/// A target that is already green — or absent — means the failure was not
/// reproduced (wrong test, or an environment issue masking it), and fixing must
/// not proceed. Returns the first offending target id on failure.
pub fn targets_reproduce(base: &Snapshot, targets: &[TestId]) -> Result<(), TestId> {
    for t in targets {
        match base.get(t) {
            Some(o) if o.is_red() => {}
            _ => return Err(t.clone()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome::*;
    use std::cell::RefCell;

    /// Replays a queue of scripted snapshots, one per `run` call.
    struct SeqRunner {
        seq: Vec<Snapshot>,
        i: RefCell<usize>,
    }
    impl TestRunner for SeqRunner {
        fn run(&self, _ids: &[TestId], _ring: Ring) -> anyhow::Result<Snapshot> {
            let mut i = self.i.borrow_mut();
            let s = self.seq[*i].clone();
            *i += 1;
            Ok(s)
        }
    }
    fn ids(v: &[&str]) -> Vec<TestId> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agreeing_runs_capture_a_baseline() {
        let runner = SeqRunner {
            seq: vec![
                Snapshot::from_pairs([("t", Fail)]),
                Snapshot::from_pairs([("t", Fail)]),
            ],
            i: RefCell::new(0),
        };
        let snap = capture_stable(&runner, &ids(&["t"]), Ring::Inner).unwrap().unwrap();
        assert_eq!(snap.get("t"), Some(Fail));
        assert_eq!(snap.runs, 2);
    }

    #[test]
    fn disagreeing_runs_are_unstable() {
        let runner = SeqRunner {
            seq: vec![
                Snapshot::from_pairs([("t", Fail)]),
                Snapshot::from_pairs([("t", Pass)]), // flaky
            ],
            i: RefCell::new(0),
        };
        assert!(capture_stable(&runner, &ids(&["t"]), Ring::Inner).unwrap().is_none());
    }

    #[test]
    fn reproduce_requires_red_targets() {
        let base = Snapshot::from_pairs([("t1", Fail), ("t2", Errored)]);
        assert!(targets_reproduce(&base, &ids(&["t1", "t2"])).is_ok());

        let green = Snapshot::from_pairs([("t1", Pass)]);
        assert_eq!(targets_reproduce(&green, &ids(&["t1"])), Err("t1".to_string()));

        let missing = Snapshot::default();
        assert_eq!(targets_reproduce(&missing, &ids(&["t1"])), Err("t1".to_string()));
    }
}
