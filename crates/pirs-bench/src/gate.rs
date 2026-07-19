//! The verification gate — the single invariant every other phase and every
//! Rhai heuristic anchors on. It is a **pure** function of captured test states
//! so its behavior is fully unit-testable without running a subprocess; the I/O
//! that captures those states lives in the driver.
//!
//! The rules, in order:
//!   1. Every target must transition red→green. A target the runner did not
//!      collect is a FAILURE, never a pass.
//!   2. Nothing green at baseline may regress. Tests already red at baseline are
//!      out of scope — the repo is not assumed green at checkout.
//!   3. Every target's flip must reproduce on a second run, rejecting a flaky
//!      red→green that a fix did not actually cause.

use crate::types::{FailBucket, Snapshot, TestId, TestOutcome};

/// The result of evaluating a candidate against its baseline. Only [`Verdict::Done`]
/// permits declaring a task solved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// All targets flipped and stayed green; no baseline-green regressed.
    Done,
    /// A target did not reach green (still red, or unexpectedly green at baseline).
    NotYet(TestId),
    /// A test green at baseline is no longer green.
    Regressed(TestId),
    /// A target was not collected by the runner — treated as failure.
    TargetNotCollected(TestId),
    /// A target's flip did not reproduce on the confirmation run.
    Flaky(TestId),
}

impl Verdict {
    pub fn is_done(&self) -> bool {
        matches!(self, Verdict::Done)
    }

    /// The failure bucket to attribute when this verdict blocks completion.
    /// `Done` has none.
    pub fn fail_bucket(&self) -> Option<FailBucket> {
        match self {
            Verdict::Done => None,
            Verdict::NotYet(_) | Verdict::TargetNotCollected(_) => Some(FailBucket::FixNoFlip),
            Verdict::Regressed(_) => Some(FailBucket::Regressed),
            Verdict::Flaky(_) => Some(FailBucket::Flaky),
        }
    }
}

/// Rules 1 & 2 of the gate over a single post-edit snapshot: every target flips
/// red→green (NotCollected is failure), and nothing green at baseline regresses.
/// Returns [`Verdict::Done`] when both hold — *provisionally*, pending the flaky
/// re-confirmation in [`confirm_flips`]. Splitting it out lets the driver skip
/// the (subprocess-costly) confirmation run when the fix hasn't landed yet.
pub fn provisional(
    targets: &[TestId],
    scope: &[TestId],
    base: &Snapshot,
    post: &Snapshot,
) -> Verdict {
    // (1) Every target must flip red→green; NotCollected/absent is failure.
    for t in targets {
        match (base.get(t), post.get(t)) {
            (Some(b), Some(TestOutcome::Pass)) if b.is_red() => {} // flipped ✔
            (_, Some(TestOutcome::NotCollected)) | (_, None) => {
                return Verdict::TargetNotCollected(t.clone());
            }
            _ => return Verdict::NotYet(t.clone()),
        }
    }
    // (2) Nothing green at baseline may regress. Baseline-red tests are ignored.
    for t in scope {
        if base.get(t) == Some(TestOutcome::Pass) && post.get(t) != Some(TestOutcome::Pass) {
            return Verdict::Regressed(t.clone());
        }
    }
    Verdict::Done
}

/// Rule 3: re-confirm every target's flip on a second run. Returns `Some(Flaky)`
/// for the first target that failed to stay green, or `None` if all held.
pub fn confirm_flips(targets: &[TestId], post2: &Snapshot) -> Option<Verdict> {
    for t in targets {
        if post2.get(t) != Some(TestOutcome::Pass) {
            return Some(Verdict::Flaky(t.clone()));
        }
    }
    None
}

/// Evaluate a candidate differentially against a baseline — the full gate in one
/// call. `post2` is a second run over `targets` only, for the flaky guard.
/// Equivalent to [`provisional`] followed by [`confirm_flips`].
pub fn evaluate(
    targets: &[TestId],
    scope: &[TestId],
    base: &Snapshot,
    post: &Snapshot,
    post2: &Snapshot,
) -> Verdict {
    let prov = provisional(targets, scope, base, post);
    if !prov.is_done() {
        return prov;
    }
    confirm_flips(targets, post2).unwrap_or(Verdict::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome::*;

    fn ids(v: &[&str]) -> Vec<TestId> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn clean_flip_is_done() {
        let targets = ids(&["t1"]);
        let scope = ids(&["t1", "keep"]);
        let base = Snapshot::from_pairs([("t1", Fail), ("keep", Pass)]);
        let post = Snapshot::from_pairs([("t1", Pass), ("keep", Pass)]);
        let post2 = Snapshot::from_pairs([("t1", Pass)]);
        assert_eq!(evaluate(&targets, &scope, &base, &post, &post2), Verdict::Done);
    }

    #[test]
    fn errored_target_flipping_is_done() {
        // A target red via error (not assertion) still counts as a valid flip.
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Errored)]);
        let post = Snapshot::from_pairs([("t1", Pass)]);
        let post2 = post.clone();
        assert_eq!(evaluate(&targets, &targets, &base, &post, &post2), Verdict::Done);
    }

    #[test]
    fn target_still_red_is_not_yet() {
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let post = Snapshot::from_pairs([("t1", Fail)]);
        assert_eq!(
            evaluate(&targets, &targets, &base, &post, &post),
            Verdict::NotYet("t1".into())
        );
    }

    #[test]
    fn deleted_target_is_not_collected_not_pass() {
        // The anti-false-green rule: a patch that removes the failing test must
        // NOT read as success.
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let post = Snapshot::from_pairs([("t1", NotCollected)]);
        assert_eq!(
            evaluate(&targets, &targets, &base, &post, &post),
            Verdict::TargetNotCollected("t1".into())
        );
    }

    #[test]
    fn missing_target_in_post_is_not_collected() {
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let post = Snapshot::default(); // runner reported nothing for t1
        assert_eq!(
            evaluate(&targets, &targets, &base, &post, &post),
            Verdict::TargetNotCollected("t1".into())
        );
    }

    #[test]
    fn regressed_neighbor_blocks_done() {
        let targets = ids(&["t1"]);
        let scope = ids(&["t1", "neighbor"]);
        let base = Snapshot::from_pairs([("t1", Fail), ("neighbor", Pass)]);
        let post = Snapshot::from_pairs([("t1", Pass), ("neighbor", Fail)]);
        let post2 = Snapshot::from_pairs([("t1", Pass)]);
        assert_eq!(
            evaluate(&targets, &scope, &base, &post, &post2),
            Verdict::Regressed("neighbor".into())
        );
    }

    #[test]
    fn preexisting_red_is_ignored() {
        // A test already red at baseline that stays red is NOT a regression —
        // the repo was not green at checkout and that failure isn't ours.
        let targets = ids(&["t1"]);
        let scope = ids(&["t1", "already_broken"]);
        let base = Snapshot::from_pairs([("t1", Fail), ("already_broken", Fail)]);
        let post = Snapshot::from_pairs([("t1", Pass), ("already_broken", Fail)]);
        let post2 = Snapshot::from_pairs([("t1", Pass)]);
        assert_eq!(evaluate(&targets, &scope, &base, &post, &post2), Verdict::Done);
    }

    #[test]
    fn flaky_flip_is_rejected() {
        // First post run flips green, but the confirmation run does not — the fix
        // didn't actually cause it.
        let targets = ids(&["t1"]);
        let base = Snapshot::from_pairs([("t1", Fail)]);
        let post = Snapshot::from_pairs([("t1", Pass)]);
        let post2 = Snapshot::from_pairs([("t1", Fail)]);
        assert_eq!(
            evaluate(&targets, &targets, &base, &post, &post2),
            Verdict::Flaky("t1".into())
        );
    }

    #[test]
    fn multiple_targets_all_must_flip() {
        let targets = ids(&["t1", "t2"]);
        let base = Snapshot::from_pairs([("t1", Fail), ("t2", Fail)]);
        // t2 never flipped.
        let post = Snapshot::from_pairs([("t1", Pass), ("t2", Fail)]);
        let post2 = Snapshot::from_pairs([("t1", Pass), ("t2", Fail)]);
        assert_eq!(
            evaluate(&targets, &targets, &base, &post, &post2),
            Verdict::NotYet("t2".into())
        );
    }

    #[test]
    fn done_verdict_has_no_fail_bucket() {
        assert_eq!(Verdict::Done.fail_bucket(), None);
        assert_eq!(
            Verdict::Regressed("x".into()).fail_bucket(),
            Some(FailBucket::Regressed)
        );
        assert_eq!(
            Verdict::TargetNotCollected("x".into()).fail_bucket(),
            Some(FailBucket::FixNoFlip)
        );
    }
}
