//! pirs-bench — a benchmark-attack harness for SWE-bench-style tasks.
//!
//! The crate is deliberately small: it owns the invariant-critical primitives
//! and the verification [`gate`], while heuristics, per-ecosystem detectors, and
//! orchestration policy live in Rhai scripts layered on top. See
//! `PIRS-BENCH-PLAN.md` for the full design.

pub mod gate;
pub mod junit;
pub mod report;
pub mod run;
pub mod types;

pub use gate::{evaluate, Verdict};
pub use report::Attribution;
pub use run::{verify, TestRunner, VerifyPlan};
pub use types::{FailBucket, Outcome, Ring, Snapshot, TestId, TestOutcome};
