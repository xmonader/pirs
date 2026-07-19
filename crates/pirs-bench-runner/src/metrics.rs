//! Token accounting and behavior stats.
//!
//! Two things every run must be able to answer: *what did it cost* (input /
//! cached / output tokens, per model) and *what did the agent actually do*
//! (turns and tool calls — the signal that separates a real fix from a model
//! that just emitted prose). Both are collected per session and aggregate.

use std::collections::HashMap;
use std::time::Duration;

use pirs_ai::pricing::PriceTable;
use pirs_ai::Usage;

/// Token usage aggregated by model id. `Usage` carries input/output plus
/// cache-read/cache-write and reasoning tokens.
#[derive(Debug, Default, Clone)]
pub struct UsageByModel {
    per_model: HashMap<String, Usage>,
}

impl UsageByModel {
    /// Fold one message's usage into the model's running total.
    pub fn add(&mut self, model: &str, u: &Usage) {
        *self.per_model.entry(model.to_string()).or_default() += u.clone();
    }

    /// Merge another accumulator in (for per-session → grand-total rollups).
    pub fn merge(&mut self, other: &UsageByModel) {
        for (model, u) in &other.per_model {
            *self.per_model.entry(model.clone()).or_default() += u.clone();
        }
    }

    /// Combined usage across all models.
    pub fn total(&self) -> Usage {
        self.per_model
            .values()
            .fold(Usage::default(), |acc, u| acc + u.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.per_model.is_empty()
    }

    /// A one-line summary for a single model's usage.
    pub fn line(u: &Usage) -> String {
        format!(
            "in={} cache_r={} cache_w={} out={} reasoning={} total={}",
            u.input, u.cache_read, u.cache_write, u.output, u.reasoning, u.total_tokens
        )
    }

    /// A multi-line report: one line per model, then a TOTAL line. When a price is
    /// known for a model, its USD cost is appended; unknown models show no cost and
    /// are excluded from the total's dollar figure (labelled as such).
    pub fn report(&self) -> String {
        let prices = PriceTable::builtin();
        let mut models: Vec<_> = self.per_model.iter().collect();
        models.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = String::from("tokens by model:\n");
        let mut priced_usd = 0.0;
        let mut any_unpriced = false;
        for (model, u) in models {
            match prices.cost(model, u) {
                Some(c) => {
                    priced_usd += c;
                    out.push_str(&format!("  {model}: {} — ${c:.4}\n", Self::line(u)));
                }
                None => {
                    any_unpriced = true;
                    out.push_str(&format!("  {model}: {} — $?\n", Self::line(u)));
                }
            }
        }
        let note = if any_unpriced {
            " (priced models only)"
        } else {
            ""
        };
        out.push_str(&format!(
            "  TOTAL: {} — ${priced_usd:.4}{note}",
            Self::line(&self.total())
        ));
        out
    }
}

/// Observed agent behavior for one session — enough to validate that the agent
/// used tools rather than just talked, and how hard it worked.
#[derive(Debug, Default, Clone)]
pub struct SessionStats {
    pub turns: u32,
    pub tool_calls: u32,
    pub tool_counts: HashMap<String, u32>,
    /// Cumulative wall-clock spent inside each tool, keyed by tool name. Note:
    /// tools may run concurrently, so the sum can exceed real elapsed time — this
    /// is "time attributable to tool X", not a partition of the clock.
    pub tool_time: HashMap<String, Duration>,
}

impl SessionStats {
    pub fn record_tool(&mut self, name: &str) {
        self.tool_calls += 1;
        *self.tool_counts.entry(name.to_string()).or_default() += 1;
    }

    /// Attribute a completed tool execution's wall-clock to its tool name.
    pub fn add_tool_time(&mut self, name: &str, d: Duration) {
        *self.tool_time.entry(name.to_string()).or_default() += d;
    }

    /// Total wall-clock attributed to tools (see the caveat on `tool_time`).
    pub fn tool_time_total(&self) -> Duration {
        self.tool_time.values().copied().sum()
    }

    /// Compact one-line summary, e.g. `turns=6 tools=9 [edit:3 read:4 bash:2]`.
    pub fn summary(&self) -> String {
        let mut tools: Vec<_> = self.tool_counts.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let breakdown = tools
            .iter()
            .map(|(n, c)| format!("{n}:{c}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "turns={} tools={} [{}]",
            self.turns, self.tool_calls, breakdown
        )
    }

    /// Per-tool wall-clock breakdown, biggest first, e.g. `bash:4.10s edit:0.02s`.
    /// Empty string when no tool timing was recorded.
    pub fn tool_time_summary(&self) -> String {
        let mut rows: Vec<_> = self.tool_time.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        rows.iter()
            .map(|(n, d)| format!("{n}:{:.2}s", d.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            total_tokens: input + output,
            ..Default::default()
        }
    }

    #[test]
    fn usage_aggregates_per_model_and_total() {
        let mut u = UsageByModel::default();
        u.add("m1", &usage(100, 50));
        u.add("m1", &usage(10, 5));
        u.add("m2", &usage(1, 1));
        let total = u.total();
        assert_eq!(total.input, 111);
        assert_eq!(total.output, 56);
        assert!(u.report().contains("m1:"));
        assert!(u.report().contains("TOTAL:"));
    }

    #[test]
    fn report_prices_known_models_and_flags_unknown() {
        let mut u = UsageByModel::default();
        // 1M output on opus @ $25/M = $25.0000.
        u.add("claude-opus-4-8", &usage(0, 1_000_000));
        let priced = u.report();
        assert!(priced.contains("$25.0000"), "{priced}");
        assert!(priced.contains("TOTAL:"));
        assert!(!priced.contains("$?"), "all models priced: {priced}");

        // An unknown model shows $? and the total is flagged partial.
        u.add("mystery-model-9", &usage(0, 1_000_000));
        let mixed = u.report();
        assert!(mixed.contains("$?"), "{mixed}");
        assert!(mixed.contains("priced models only"), "{mixed}");
    }

    #[test]
    fn merge_folds_sessions_together() {
        let mut a = UsageByModel::default();
        a.add("m", &usage(5, 5));
        let mut b = UsageByModel::default();
        b.add("m", &usage(3, 2));
        a.merge(&b);
        assert_eq!(a.total().input, 8);
        assert_eq!(a.total().output, 7);
    }

    #[test]
    fn stats_summary_counts_tools() {
        let mut s = SessionStats {
            turns: 3,
            ..Default::default()
        };
        s.record_tool("edit");
        s.record_tool("edit");
        s.record_tool("read");
        assert_eq!(s.tool_calls, 3);
        let sum = s.summary();
        assert!(sum.contains("turns=3"));
        assert!(sum.contains("edit:2"));
    }
}
