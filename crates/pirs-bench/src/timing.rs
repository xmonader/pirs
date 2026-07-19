//! Phase timing — "where did every second go?"
//!
//! The harness records the wall-clock of each non-overlapping phase (discovery,
//! bootstrap, baseline, the agent's fix, verification, patch/rollback) so a run
//! can be broken down instead of reported as one opaque duration. Phases are
//! summed by label, so the fix/verify phases that repeat per attempt roll up
//! with a count.

use std::time::{Duration, Instant};

/// An ordered accumulator of `(phase label, duration)` samples.
#[derive(Debug, Default, Clone)]
pub struct Timings {
    samples: Vec<(String, Duration)>,
}

impl Timings {
    pub fn new() -> Self {
        Timings::default()
    }

    /// Time `f`, record its wall-clock under `label`, and return its result.
    pub fn time<T>(&mut self, label: &str, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.samples.push((label.to_string(), start.elapsed()));
        out
    }

    /// Record a pre-measured duration under `label`.
    pub fn record(&mut self, label: &str, d: Duration) {
        self.samples.push((label.to_string(), d));
    }

    /// Total wall-clock across all recorded phases.
    pub fn total(&self) -> Duration {
        self.samples.iter().map(|(_, d)| *d).sum()
    }

    /// Per-label rollup: `(label, summed duration, sample count)`, sorted by
    /// duration descending (biggest cost first).
    pub fn by_label(&self) -> Vec<(String, Duration, usize)> {
        let mut order: Vec<String> = Vec::new();
        let mut sums: std::collections::HashMap<String, (Duration, usize)> =
            std::collections::HashMap::new();
        for (label, d) in &self.samples {
            let e = sums.entry(label.clone()).or_insert_with(|| {
                order.push(label.clone());
                (Duration::ZERO, 0)
            });
            e.0 += *d;
            e.1 += 1;
        }
        let mut rows: Vec<(String, Duration, usize)> =
            order.into_iter().map(|l| { let (d, n) = sums[&l]; (l, d, n) }).collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        rows
    }

    /// A human-readable breakdown: one line per phase with its share of total.
    pub fn report(&self) -> String {
        let total = self.total();
        let total_s = total.as_secs_f64().max(f64::MIN_POSITIVE);
        let mut out = format!("timing (total {:.2}s):\n", total.as_secs_f64());
        for (label, d, n) in self.by_label() {
            let pct = 100.0 * d.as_secs_f64() / total_s;
            let count = if n > 1 { format!(" n={n}") } else { String::new() };
            out.push_str(&format!("  {label}: {:.2}s ({pct:.0}%{count})\n", d.as_secs_f64()));
        }
        out.trim_end().to_string()
    }

    /// Merge another set of samples in (for aggregating across a batch).
    pub fn merge(&mut self, other: &Timings) {
        self.samples.extend(other.samples.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_records_and_returns_value() {
        let mut t = Timings::new();
        let v = t.time("phase", || 42);
        assert_eq!(v, 42);
        assert_eq!(t.by_label().len(), 1);
    }

    #[test]
    fn same_label_sums_with_count() {
        let mut t = Timings::new();
        t.record("verify", Duration::from_millis(100));
        t.record("verify", Duration::from_millis(50));
        t.record("fix", Duration::from_millis(200));
        let rows = t.by_label();
        // Sorted by duration desc: fix (200ms) before verify (150ms).
        assert_eq!(rows[0].0, "fix");
        assert_eq!(rows[1].0, "verify");
        assert_eq!(rows[1].1, Duration::from_millis(150));
        assert_eq!(rows[1].2, 2); // two verify samples
        assert_eq!(t.total(), Duration::from_millis(350));
    }

    #[test]
    fn report_shows_shares() {
        let mut t = Timings::new();
        t.record("a", Duration::from_millis(750));
        t.record("b", Duration::from_millis(250));
        let r = t.report();
        assert!(r.contains("total 1.00s"));
        assert!(r.contains("a: 0.75s (75%)"));
        assert!(r.contains("b: 0.25s (25%)"));
    }
}
