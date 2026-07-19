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
use crate::detect::{discover, DetectorHost, Discovery};
use crate::driver::{run_task, run_task_cached, Executor, TaskSpec};
use crate::git::GitWorkspace;
use crate::types::{FailBucket, Outcome, TestId};

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
}

/// Run one instance to an [`InstanceReport`]. `cache` is threaded through so a
/// batch of instances at the same checkout reuses baselines. When `workspace` is
/// `Some`, an accepted outcome yields the fix as a patch and any other outcome
/// rolls the tree back to pristine — so a failed attempt never leaves partial
/// edits on disk.
pub fn run_instance(
    inst: &Instance,
    host: &DetectorHost,
    cache: &mut BaselineCache,
    executor: &mut dyn Executor,
    max_attempts: u32,
    workspace: Option<&GitWorkspace>,
) -> anyhow::Result<InstanceReport> {
    let bail = |outcome| Ok(InstanceReport { outcome, patch: None });

    // 1. Discover and probe-confirm a runner. No confirmed runner → we can't get
    //    a pass/fail signal at all. (No edits yet, so no rollback needed.)
    let spec = match discover(host, &inst.repo_root)? {
        Discovery::Confirmed { spec, .. } => spec,
        Discovery::Unconfirmed { tried, hint } => {
            tracing::warn!("no runner confirmed ({tried} tried): {hint}");
            return bail(Outcome::Failed(FailBucket::RunnerUndetected));
        }
    };

    // 2. Make the environment usable (install + re-probe). A broken env is a
    //    distinct failure from an undetected runner.
    match bootstrap(&spec, &inst.repo_root)? {
        Bootstrap::Ready(_) => {}
        Bootstrap::Failed(hint) => {
            tracing::warn!("environment setup failed: {hint}");
            return bail(Outcome::Failed(FailBucket::EnvSetup));
        }
    }

    // 3. Build the concrete runner and drive the task. The executor edits the
    //    real tree; the outcome decides whether we keep or discard those edits.
    let runner = CommandRunner::new(spec, inst.repo_root.clone());
    let task = TaskSpec { targets: inst.targets.clone(), keep_green: inst.keep_green.clone() };

    let outcome = match &inst.base_sha {
        Some(sha) => run_task_cached(&task, &runner, executor, max_attempts, cache, sha)?,
        None => run_task(&task, &runner, executor, max_attempts)?,
    };

    // 4. Keep the fix (as a patch) or roll back to pristine.
    let patch = match workspace {
        Some(ws) if outcome.is_accepted() => Some(ws.diff()?),
        Some(ws) => {
            ws.reset()?; // discard partial/failed edits
            None
        }
        None => None,
    };

    Ok(InstanceReport { outcome, patch })
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
        let outcome =
            run_instance(&inst, &host, &mut cache, &mut NeverExecutor, 3).unwrap();
        assert_eq!(outcome, Outcome::Failed(FailBucket::RunnerUndetected));
    }
}
