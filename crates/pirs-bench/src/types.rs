//! Core value types shared across the harness. These are deliberately plain
//! data — the invariant logic that consumes them lives in [`crate::gate`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A test identifier as the runner reports it (e.g. a pytest node id
/// `tests/test_x.py::test_case`, a Go `pkg.TestName`, a Rust module path).
pub type TestId = String;

/// The outcome of a single test in one run.
///
/// `NotCollected` is a first-class, load-bearing state: a test that the runner
/// never selected is **not** a pass. Collapsing it into success is exactly how a
/// harness ships a patch that deleted the failing test and calls it green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOutcome {
    Pass,
    Fail,
    Errored,
    NotCollected,
}

impl TestOutcome {
    /// A test the runner never selected — treated as failure, never pass.
    pub fn is_collected(self) -> bool {
        !matches!(self, TestOutcome::NotCollected)
    }

    /// A red state we expect a target to start in and a fix to move away from.
    pub fn is_red(self) -> bool {
        matches!(self, TestOutcome::Fail | TestOutcome::Errored)
    }
}

/// A captured test-state snapshot for one [`Ring`], used as the baseline that
/// verification is diffed against. Cached by `(test_id, base_sha)` upstream so a
/// task never re-runs an unchanged ring.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub states: HashMap<TestId, TestOutcome>,
    /// Whether the project built/imported cleanly when the snapshot was taken.
    pub build_ok: bool,
    /// How many times the set was run to establish this snapshot (≥2 confirms
    /// stability against flakiness).
    pub runs: u8,
}

impl Snapshot {
    /// Build a snapshot from `(id, outcome)` pairs. `build_ok` defaults to true
    /// and `runs` to 1 for the common single-run case.
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, TestOutcome)>,
        S: Into<TestId>,
    {
        Snapshot {
            states: pairs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            build_ok: true,
            runs: 1,
        }
    }

    pub fn get(&self, id: &str) -> Option<TestOutcome> {
        self.states.get(id).copied()
    }
}

/// How to install, probe, and run a project's tests. Produced by a Rhai
/// detector (heuristic, per-ecosystem) and consumed by the Rust runner. The
/// `test_cmd` template carries two placeholders:
///   * `{tests}` — the test ids to run, joined by `test_join` (empty ⇒ whole suite);
///   * `{junit}` — the path the run must write JUnit XML to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSpec {
    pub framework: String,
    /// Commands to establish a usable environment (editable install, build).
    #[serde(default)]
    pub install: Vec<String>,
    /// Collect-only / dry-run that proves the runner works (lists tests).
    pub list_cmd: String,
    /// Test command template with `{tests}` and `{junit}` placeholders.
    pub test_cmd: String,
    /// Separator for joining multiple test ids into `{tests}`. Defaults to a
    /// space (pytest node ids); Go's `-run` takes a regex, so its detector sets
    /// `"|"` to form an alternation instead of a space-containing regex that
    /// matches nothing.
    #[serde(default = "default_test_join")]
    pub test_join: String,
    /// Per-run wall-clock budget in seconds.
    pub timeout_secs: u64,
    #[serde(default)]
    pub parallel: bool,
}

fn default_test_join() -> String {
    " ".to_string()
}

/// Concentric cost tiers. Verification runs the smallest ring that answers the
/// current question; the full suite is paid at most once per task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ring {
    /// Targets only — the tight reproduce/fix/refine loop. Seconds.
    Inner,
    /// Tests reaching the edited modules — the regression check before accepting.
    Scoped,
    /// The full keep-green set — the end-of-task backstop.
    Full,
}

/// The terminal result of a single task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Targets flipped and the full keep-green set verified clean.
    Solved,
    /// Targets flipped and the scoped ring verified, but the full ring was not
    /// completed (e.g. budget/timeout). Weaker evidence — flagged, not silent.
    AcceptedScopedOnly,
    /// Aborted, with the typed reason.
    Failed(FailBucket),
}

/// The typed reason a task aborted, recorded for every failure so the aggregate
/// histogram shows *where* the harness loses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailBucket {
    /// Couldn't establish a stable pass/fail signal at checkout.
    BaselineUnusable,
    /// No test runner could be detected and probe-confirmed.
    RunnerUndetected,
    /// A target did not fail at baseline for the issue's reason.
    ReproFailed,
    /// Localization never surfaced a plausible edit site.
    LocalizeMiss,
    /// A fix never made the target(s) go red→green.
    FixNoFlip,
    /// A previously-green test went red.
    Regressed,
    /// A target's red→green flip did not reproduce on a second run.
    Flaky,
    /// A ring exceeded its wall-clock budget.
    Timeout,
    /// Environment setup could not be made to work.
    EnvSetup,
}
