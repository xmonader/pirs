//! pirs-bench — a benchmark-attack harness for SWE-bench-style tasks.
//!
//! The crate is deliberately small: it owns the invariant-critical primitives
//! and the verification [`gate`], while heuristics, per-ecosystem detectors, and
//! orchestration policy live in Rhai scripts layered on top. See
//! `PIRS-BENCH-PLAN.md` for the full design.

pub mod baseline;
pub mod bootstrap;
pub mod cache;
pub mod command;
pub mod detect;
pub mod driver;
pub mod gate;
pub mod harness;
pub mod junit;
pub mod localize;
pub mod orchestrate;
pub mod probe;
pub mod proc;
pub mod report;
pub mod run;
pub mod types;

pub use baseline::{capture_stable, capture_stable_cached, targets_reproduce};
pub use cache::BaselineCache;
pub use bootstrap::{bootstrap, Bootstrap};
pub use command::CommandRunner;
pub use detect::{discover, DetectorHost, Discovery};
pub use driver::{run_task, run_task_cached, Executor, TaskSpec};
pub use harness::{run_instance, Instance};
pub use gate::{evaluate, Verdict};
pub use localize::{parse_traceback, rank_candidates, scoped_tests, Candidate, Frame};
pub use orchestrate::{plan_next, steer_hint, Hint, ModelOracle, PlanDecision};
pub use probe::{probe, ProbeResult};
pub use report::Attribution;
pub use run::{verify, TestRunner, VerifyPlan};
pub use types::{FailBucket, Outcome, Ring, RunnerSpec, Snapshot, TestId, TestOutcome};
