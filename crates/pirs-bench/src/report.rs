//! Aggregate attribution — the histogram that answers the only question that
//! matters across a benchmark run: *where are we losing?* Bucketing failures by
//! phase is the roadmap; investing anywhere else is guessing.

use std::collections::BTreeMap;

use crate::types::{FailBucket, Outcome};

/// Accumulates per-task [`Outcome`]s into a solve rate and a failure histogram.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    total: u32,
    solved: u32,
    accepted_scoped_only: u32,
    /// Ordered so the rendered report is deterministic.
    buckets: BTreeMap<FailBucket, u32>,
}

impl Attribution {
    pub fn new() -> Self {
        Attribution::default()
    }

    pub fn record(&mut self, outcome: &Outcome) {
        self.total += 1;
        match outcome {
            Outcome::Solved => self.solved += 1,
            Outcome::AcceptedScopedOnly => self.accepted_scoped_only += 1,
            Outcome::Failed(bucket) => {
                *self.buckets.entry(*bucket).or_insert(0) += 1;
            }
        }
    }

    pub fn total(&self) -> u32 {
        self.total
    }

    /// Fraction of tasks that fully solved. `AcceptedScopedOnly` is *not* counted
    /// as solved — its verification is weaker and must not inflate the headline.
    pub fn solve_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.solved as f64 / self.total as f64
        }
    }

    pub fn count(&self, bucket: FailBucket) -> u32 {
        self.buckets.get(&bucket).copied().unwrap_or(0)
    }

    /// A deterministic, human-readable histogram. `report.rhai` may reformat
    /// this; the numbers come from here.
    pub fn report(&self) -> String {
        let mut out = format!(
            "tasks: {}  solved: {} ({:.1}%)  scoped-only: {}\n",
            self.total,
            self.solved,
            self.solve_rate() * 100.0,
            self.accepted_scoped_only,
        );
        if self.buckets.is_empty() {
            out.push_str("failures: none\n");
        } else {
            out.push_str("failures:\n");
            for (bucket, n) in &self.buckets {
                out.push_str(&format!("  {bucket:?}: {n}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FailBucket::*;

    #[test]
    fn solve_rate_excludes_scoped_only() {
        let mut a = Attribution::new();
        a.record(&Outcome::Solved);
        a.record(&Outcome::Solved);
        a.record(&Outcome::AcceptedScopedOnly);
        a.record(&Outcome::Failed(EnvSetup));
        assert_eq!(a.total(), 4);
        // 2 of 4 fully solved; scoped-only does not count toward the headline.
        assert!((a.solve_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn buckets_accumulate() {
        let mut a = Attribution::new();
        a.record(&Outcome::Failed(EnvSetup));
        a.record(&Outcome::Failed(EnvSetup));
        a.record(&Outcome::Failed(BaselineUnusable));
        assert_eq!(a.count(EnvSetup), 2);
        assert_eq!(a.count(BaselineUnusable), 1);
        assert_eq!(a.count(Flaky), 0);
    }

    #[test]
    fn empty_report_is_stable() {
        let a = Attribution::new();
        assert!((a.solve_rate() - 0.0).abs() < 1e-9);
        assert!(a.report().contains("failures: none"));
    }

    #[test]
    fn report_lists_buckets() {
        let mut a = Attribution::new();
        a.record(&Outcome::Failed(EnvSetup));
        let r = a.report();
        assert!(r.contains("EnvSetup: 1"), "{r}");
        assert!(r.contains("tasks: 1"), "{r}");
    }
}
