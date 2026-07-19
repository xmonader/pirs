//! pirs-bench — a benchmark-attack harness for SWE-bench-style tasks.
//!
//! The crate is deliberately small: it owns the invariant-critical primitives
//! and the verification [`gate`], while heuristics, per-ecosystem detectors, and
//! orchestration policy live in Rhai scripts layered on top. See
//! `PIRS-BENCH-PLAN.md` for the full design.

pub mod baseline;
pub mod bootstrap;
pub mod command;
pub mod detect;
pub mod driver;
pub mod gate;
pub mod junit;
pub mod probe;
pub mod proc;
pub mod report;
pub mod run;
pub mod types;

pub use baseline::{capture_stable, targets_reproduce};
pub use bootstrap::{bootstrap, Bootstrap};
pub use command::CommandRunner;
pub use detect::{discover, DetectorHost, Discovery};
pub use driver::{run_task, Executor, TaskSpec};
pub use gate::{evaluate, Verdict};
pub use probe::{probe, ProbeResult};
pub use report::Attribution;
pub use run::{verify, TestRunner, VerifyPlan};
pub use types::{FailBucket, Outcome, Ring, RunnerSpec, Snapshot, TestId, TestOutcome};
