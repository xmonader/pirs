//! Session statistics — token usage, wall clock, agent active time.
//!
//! Printed on REPL/TUI exit (and via `/stats`) so a session ends with a clear
//! summary similar to other coding CLIs (qwen-code style "away" summary).

use std::time::{Duration, Instant};

use pirs_agent::usage::UsageReport;
use pirs_ai::pricing::PriceTable;
use pirs_ai::Message;

/// Accumulates session-level timers (wall + agent-busy).
#[derive(Debug, Clone)]
pub struct SessionClock {
    started: Instant,
    /// Sum of completed agent-busy intervals.
    agent_busy: Duration,
    /// When the current busy interval started (if any).
    busy_since: Option<Instant>,
    user_turns: u32,
    tool_calls: u32,
    tool_errors: u32,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionClock {
    pub fn new() -> Self {
        SessionClock {
            started: Instant::now(),
            agent_busy: Duration::ZERO,
            busy_since: None,
            user_turns: 0,
            tool_calls: 0,
            tool_errors: 0,
        }
    }

    pub fn mark_user_turn(&mut self) {
        self.user_turns = self.user_turns.saturating_add(1);
    }

    pub fn mark_tool(&mut self, is_error: bool) {
        self.tool_calls = self.tool_calls.saturating_add(1);
        if is_error {
            self.tool_errors = self.tool_errors.saturating_add(1);
        }
    }

    /// Count tools from a message list (e.g. after a strategy turn).
    pub fn absorb_messages(&mut self, messages: &[Message]) {
        for m in messages {
            if let Message::ToolResult(tr) = m {
                self.mark_tool(tr.is_error);
            }
        }
    }

    pub fn agent_start(&mut self) {
        if self.busy_since.is_none() {
            self.busy_since = Some(Instant::now());
        }
    }

    pub fn agent_end(&mut self) {
        if let Some(since) = self.busy_since.take() {
            self.agent_busy += since.elapsed();
        }
    }

    pub fn wall(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn agent_wall(&self) -> Duration {
        let mut total = self.agent_busy;
        if let Some(since) = self.busy_since {
            total += since.elapsed();
        }
        total
    }

    pub fn user_turns(&self) -> u32 {
        self.user_turns
    }

    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    pub fn tool_errors(&self) -> u32 {
        self.tool_errors
    }
}

/// Format a duration as `1h 2m 3.4s` / `2m 3s` / `3.4s`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 3600.0 {
        let h = (secs / 3600.0).floor() as u64;
        let m = ((secs % 3600.0) / 60.0).floor() as u64;
        let s = secs % 60.0;
        format!("{h}h {m}m {s:.1}s")
    } else if secs >= 60.0 {
        let m = (secs / 60.0).floor() as u64;
        let s = secs % 60.0;
        format!("{m}m {s:.1}s")
    } else {
        format!("{secs:.1}s")
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn model_matches(recorded: &str, want: &str) -> bool {
    if recorded == want {
        return true;
    }
    let bare = recorded.strip_prefix("delegate:").unwrap_or(recorded);
    bare == want || bare.ends_with(&format!("/{want}")) || want.ends_with(&format!("/{bare}"))
}

/// Look up usage + call count for a model pin (exact, backend/id, or delegate:).
fn usage_for_model(report: &UsageReport, model: &str) -> (u64, u64, u64, usize) {
    let mut input = 0u64;
    let mut output = 0u64;
    let mut total = 0u64;
    for (m, u) in &report.by_model {
        if model_matches(m, model) {
            input += u.input + u.cache_read;
            output += u.output;
            total += u
                .total_tokens
                .max(u.input + u.output + u.cache_read + u.cache_write);
        }
    }
    let calls = report
        .calls
        .iter()
        .filter(|c| model_matches(&c.model, model))
        .count();
    (input, output, total, calls)
}

/// Hybrid plan/strategy pins for every session-end report builder.
///
/// Resolve once from CLI (or TUI state) and pass through to print sites so
/// one-shot / REPL / TUI cannot hardcode empty plan-model or strategy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportPins {
    pub plan_model: Option<String>,
    pub strategy: Option<String>,
}

impl ReportPins {
    pub fn new(plan_model: Option<String>, strategy: Option<String>) -> Self {
        Self {
            plan_model,
            strategy,
        }
    }

    /// CLI: `--strategy` wins; otherwise the profile name labels the strategy.
    pub fn from_cli(
        plan_model: Option<String>,
        strategy: Option<String>,
        profile: Option<String>,
    ) -> Self {
        Self {
            plan_model,
            strategy: strategy.or(profile),
        }
    }

    pub fn plan_model(&self) -> Option<&str> {
        self.plan_model.as_deref()
    }

    pub fn strategy(&self) -> Option<&str> {
        self.strategy.as_deref()
    }
}

/// Format plan-vs-exec role split lines for hybrid economics reporting.
///
/// When `plan_model` differs from the executor model, emit a `by role` section
/// with token totals (and dollars when the builtin price table knows the rates).
/// **Single source** for by-role strings — one-shot and session-stats builders
/// must call this rather than inventing their own planner/executor templates.
pub fn format_role_split_lines(
    report: &UsageReport,
    exec_model: &str,
    plan_model: &str,
) -> Vec<String> {
    if plan_model.is_empty() || exec_model.is_empty() || plan_model == exec_model {
        return Vec::new();
    }
    let prices = PriceTable::builtin();
    let (p_in, p_out, p_tot, p_calls) = usage_for_model(report, plan_model);
    let (e_in, e_out, e_tot, e_calls) = usage_for_model(report, exec_model);
    let p_cost_s = report
        .by_model
        .iter()
        .find(|(m, _)| model_matches(m, plan_model))
        .and_then(|(m, u)| prices.cost(m, u))
        .map(|c| format!("  ${c:.4}"))
        .unwrap_or_default();
    let e_cost_s = report
        .by_model
        .iter()
        .find(|(m, _)| model_matches(m, exec_model))
        .and_then(|(m, u)| prices.cost(m, u))
        .map(|c| format!("  ${c:.4}"))
        .unwrap_or_default();
    vec![
        "  by role".into(),
        format!(
            "    planner ({plan_model})  ×{p_calls}  in {}  out {}  total {}{p_cost_s}",
            format_tokens(p_in),
            format_tokens(p_out),
            format_tokens(p_tot),
        ),
        format!(
            "    executor ({exec_model})  ×{e_calls}  in {}  out {}  total {}{e_cost_s}",
            format_tokens(e_in),
            format_tokens(e_out),
            format_tokens(e_tot),
        ),
    ]
}

/// Multi-line session summary for stderr / post-TUI stdout.
pub fn format_session_stats(
    clock: &SessionClock,
    report: &UsageReport,
    model: &str,
    plan_model: Option<&str>,
    strategy: Option<&str>,
) -> String {
    let total = report.grand_total();
    let wall = clock.wall();
    let agent = clock.agent_wall();
    let idle = wall.saturating_sub(agent);
    let hit = if total.input + total.cache_read > 0 {
        100.0 * total.cache_read as f64 / (total.input + total.cache_read) as f64
    } else {
        0.0
    };

    let prices = PriceTable::builtin();
    let mut cost_total = 0.0f64;
    let mut cost_known = true;
    for (m, u) in &report.by_model {
        match prices.cost(m, u) {
            Some(c) => cost_total += c,
            None => cost_known = false,
        }
    }

    let mut lines = Vec::new();
    lines.push("── session stats ─────────────────────────────".into());
    lines.push(format!("  wall time      {}", format_duration(wall)));
    lines.push(format!(
        "  agent time     {}  (idle {})",
        format_duration(agent),
        format_duration(idle)
    ));
    lines.push(format!("  user turns     {}", clock.user_turns()));
    lines.push(format!(
        "  api calls      {}  ({} delegate)",
        report.calls.len().saturating_sub(report.delegate_calls()),
        report.delegate_calls()
    ));
    if clock.tool_calls() > 0 {
        lines.push(format!(
            "  tool calls     {}  ({} error{})",
            clock.tool_calls(),
            clock.tool_errors(),
            if clock.tool_errors() == 1 { "" } else { "s" }
        ));
    }
    lines.push(format!(
        "  tokens         in {}  ·  out {}  ·  cache {} ({:.0}%)  ·  total {}",
        format_tokens(total.input),
        format_tokens(total.output),
        format_tokens(total.cache_read),
        hit,
        format_tokens(total.total_tokens)
    ));
    if total.reasoning > 0 {
        lines.push(format!(
            "  reasoning      {}",
            format_tokens(total.reasoning)
        ));
    }
    if cost_known && cost_total > 0.0 {
        lines.push(format!(
            "  est. cost      ${cost_total:.4}  (builtin price table)"
        ));
    } else if !report.by_model.is_empty() {
        lines.push("  est. cost      n/a  (unknown model rates)".into());
    }
    lines.push(format!("  model          {model}"));
    if let Some(p) = plan_model {
        lines.push(format!("  plan-model     {p}"));
    }
    if let Some(s) = strategy {
        lines.push(format!("  strategy       {s}"));
    }
    if let Some(pm) = plan_model {
        lines.extend(format_role_split_lines(report, model, pm));
    }
    if !report.by_model.is_empty() {
        lines.push("  by model".into());
        for (m, u) in &report.by_model {
            let calls = report
                .calls
                .iter()
                .filter(|c| c.model == *m || c.model == format!("delegate:{m}"))
                .count();
            let c = prices.cost(m, u);
            let cost_s = c.map(|x| format!("  ${x:.4}")).unwrap_or_default();
            lines.push(format!(
                "    {m}  ×{calls}  in {}  out {}{cost_s}",
                format_tokens(u.input + u.cache_read),
                format_tokens(u.output),
            ));
        }
    }
    lines.push("──────────────────────────────────────────────".into());
    lines.join("\n")
}

/// Session stats text using resolved [`ReportPins`] (REPL / TUI /stats path).
pub fn format_session_stats_pins(
    clock: &SessionClock,
    report: &UsageReport,
    model: &str,
    pins: &ReportPins,
) -> String {
    format_session_stats(clock, report, model, pins.plan_model(), pins.strategy())
}

/// Print session stats to stderr (raw plan/strategy args).
/// Prefer [`print_session_stats_pins`] at product exit sites.
pub fn print_session_stats(
    clock: &SessionClock,
    report: &UsageReport,
    model: &str,
    plan_model: Option<&str>,
    strategy: Option<&str>,
) {
    let text = format_session_stats(clock, report, model, plan_model, strategy);
    eprintln!("\n{text}");
}

/// Print session stats to stderr using resolved [`ReportPins`] (no pin drop).
/// Preferred exit API for REPL session-end, TUI session-end, and `/stats`.
pub fn print_session_stats_pins(
    clock: &SessionClock,
    report: &UsageReport,
    model: &str,
    pins: &ReportPins,
) {
    print_session_stats(clock, report, model, pins.plan_model(), pins.strategy());
}

/// One-shot / compact usage footer (strategy and mono one-shot exits).
///
/// When `pins.plan_model` differs from `model`, appends the shared
/// [`format_role_split_lines`] plan-vs-exec **by role** block.
pub fn format_usage_end(
    report: &UsageReport,
    model: &str,
    plan_model: Option<&str>,
    strategy: Option<&str>,
) -> String {
    let total = report.grand_total();
    let hit_rate = if total.input + total.cache_read > 0 {
        100.0 * total.cache_read as f64 / (total.input + total.cache_read) as f64
    } else {
        0.0
    };
    let mut lines = Vec::new();
    lines.push(format!(
        "[usage: {} api calls + {} delegate sub-agents | input {} (cached {}, {:.0}%) | output {} | reasoning {} | total {}]",
        report.calls.len() - report.delegate_calls(),
        report.delegate_calls(),
        total.input,
        total.cache_read,
        hit_rate,
        total.output,
        total.reasoning,
        total.total_tokens,
    ));
    if let Some(s) = strategy {
        lines.push(format!("  strategy       {s}"));
    }
    if let Some(pm) = plan_model {
        lines.push(format!("  model          {model}"));
        lines.push(format!("  plan-model     {pm}"));
        lines.extend(format_role_split_lines(report, model, pm));
    }
    // Per-model lines make strong-plan / weak-exec splits visible at a glance.
    for (m, u) in &report.by_model {
        let calls = report.calls.iter().filter(|c| c.model == *m).count();
        lines.push(format!(
            "  {m} ({calls} call{}): input {} (cached {}) output {} total {}",
            if calls == 1 { "" } else { "s" },
            u.input,
            u.cache_read,
            u.output,
            u.total_tokens
        ));
    }
    lines.join("\n")
}

/// Compact footer using resolved [`ReportPins`].
pub fn format_usage_end_pins(report: &UsageReport, model: &str, pins: &ReportPins) -> String {
    format_usage_end(report, model, pins.plan_model(), pins.strategy())
}

/// Print the one-shot usage footer to stderr.
pub fn print_usage_end(report: &UsageReport, model: &str, pins: &ReportPins) {
    eprintln!("{}", format_usage_end_pins(report, model, pins));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::{ContentBlock, Usage};

    #[test]
    fn format_duration_ranges() {
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
        assert!(format_duration(Duration::from_secs(65)).starts_with("1m"));
        assert!(format_duration(Duration::from_secs(3661)).contains('h'));
    }

    #[test]
    fn clock_tracks_busy_and_tools() {
        let mut c = SessionClock::new();
        c.mark_user_turn();
        c.agent_start();
        std::thread::sleep(Duration::from_millis(20));
        c.agent_end();
        c.mark_tool(false);
        c.mark_tool(true);
        assert_eq!(c.user_turns(), 1);
        assert_eq!(c.tool_calls(), 2);
        assert_eq!(c.tool_errors(), 1);
        assert!(c.agent_wall() >= Duration::from_millis(15));
        assert!(c.wall() >= c.agent_wall());
    }

    #[test]
    fn format_includes_tokens_and_wall() {
        let clock = SessionClock::new();
        let mut report = UsageReport::default();
        report.calls.push(pirs_agent::usage::UsageRecord {
            model: "deepseek-v4-flash".into(),
            usage: Usage {
                input: 1000,
                output: 200,
                cache_read: 100,
                total_tokens: 1200,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            timestamp: 0,
        });
        report.main_usage.input = 1000;
        report.main_usage.output = 200;
        report.main_usage.cache_read = 100;
        *report
            .by_model
            .entry("deepseek-v4-flash".into())
            .or_default() = Usage {
            input: 1000,
            output: 200,
            cache_read: 100,
            ..Default::default()
        };
        let s = format_session_stats(
            &clock,
            &report,
            "qwen3.5-plus",
            Some("deepseek-v4-pro"),
            Some("plan-exec"),
        );
        assert!(s.contains("session stats"));
        assert!(s.contains("wall time"));
        assert!(s.contains("agent time"));
        assert!(s.contains("tokens"));
        assert!(s.contains("deepseek-v4-flash"));
        assert!(s.contains("plan-exec"));
        assert!(s.contains("qwen3.5-plus"));
        assert!(
            s.contains("by role"),
            "hybrid report must include by-role split: {s}"
        );
        assert!(s.contains("planner"), "{s}");
        assert!(s.contains("executor"), "{s}");
    }

    #[test]
    fn role_split_attributes_plan_and_exec_usage() {
        let mut report = UsageReport::default();
        report.calls.push(pirs_agent::usage::UsageRecord {
            model: "strong-planner".into(),
            usage: Usage {
                input: 500,
                output: 100,
                total_tokens: 600,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            timestamp: 0,
        });
        report.calls.push(pirs_agent::usage::UsageRecord {
            model: "weak-executor".into(),
            usage: Usage {
                input: 9000,
                output: 400,
                total_tokens: 9400,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            timestamp: 1,
        });
        *report.by_model.entry("strong-planner".into()).or_default() = Usage {
            input: 500,
            output: 100,
            total_tokens: 600,
            ..Default::default()
        };
        *report.by_model.entry("weak-executor".into()).or_default() = Usage {
            input: 9000,
            output: 400,
            total_tokens: 9400,
            ..Default::default()
        };
        let lines = format_role_split_lines(&report, "weak-executor", "strong-planner");
        let text = lines.join("\n");
        assert!(text.contains("by role"), "{text}");
        assert!(text.contains("planner (strong-planner)"), "{text}");
        assert!(text.contains("executor (weak-executor)"), "{text}");
        // Executor should show the larger token total.
        assert!(
            text.contains("9.4k") || text.contains("9400") || text.contains("9.0k"),
            "{text}"
        );
        let full = format_session_stats(
            &SessionClock::new(),
            &report,
            "weak-executor",
            Some("strong-planner"),
            Some("plan-exec"),
        );
        assert!(full.contains("by role") && full.contains("planner") && full.contains("executor"));
    }

    fn hybrid_multi_model_report() -> UsageReport {
        let mut report = UsageReport::default();
        report.calls.push(pirs_agent::usage::UsageRecord {
            model: "strong-planner".into(),
            usage: Usage {
                input: 1000,
                output: 200,
                total_tokens: 1200,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            timestamp: 0,
        });
        report.calls.push(pirs_agent::usage::UsageRecord {
            model: "weak-executor".into(),
            usage: Usage {
                input: 500,
                output: 100,
                total_tokens: 600,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            timestamp: 1,
        });
        *report.by_model.entry("strong-planner".into()).or_default() = Usage {
            input: 1000,
            output: 200,
            total_tokens: 1200,
            ..Default::default()
        };
        *report.by_model.entry("weak-executor".into()).or_default() = Usage {
            input: 500,
            output: 100,
            total_tokens: 600,
            ..Default::default()
        };
        report
    }

    /// Shipped one-shot footer builder — same function `print_usage_end` uses.
    #[test]
    fn format_usage_end_hybrid_includes_by_role() {
        let report = hybrid_multi_model_report();
        let pins = ReportPins::from_cli(
            Some("strong-planner".into()),
            Some("plan-exec".into()),
            None,
        );
        let text = format_usage_end_pins(&report, "weak-executor", &pins);
        assert!(
            text.contains("by role"),
            "one-shot hybrid footer must include by role:\n{text}"
        );
        assert!(text.contains("planner"), "{text}");
        assert!(text.contains("executor"), "{text}");
        assert!(text.contains("strong-planner"), "{text}");
        assert!(text.contains("weak-executor"), "{text}");
        assert!(text.contains("plan-exec"), "{text}");
        assert!(text.contains("plan-model"), "{text}");
    }

    #[test]
    fn format_usage_end_without_plan_model_skips_role_split() {
        let mut report = UsageReport::default();
        report.calls.push(pirs_agent::usage::UsageRecord {
            model: "only-model".into(),
            usage: Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            timestamp: 0,
        });
        *report.by_model.entry("only-model".into()).or_default() = Usage {
            input: 10,
            output: 5,
            total_tokens: 15,
            ..Default::default()
        };
        let text = format_usage_end(&report, "only-model", None, None);
        assert!(!text.contains("by role"), "{text}");
        assert!(!text.contains("plan-model"), "{text}");
    }

    /// Session-stats builder (REPL/TUI) and one-shot footer share role-split output.
    #[test]
    fn one_shot_and_session_stats_share_role_split_lines() {
        let report = hybrid_multi_model_report();
        let pins = ReportPins::new(Some("strong-planner".into()), Some("plan-exec".into()));
        let footer = format_usage_end_pins(&report, "weak-executor", &pins);
        let session =
            format_session_stats_pins(&SessionClock::new(), &report, "weak-executor", &pins);
        let shared = format_role_split_lines(&report, "weak-executor", "strong-planner");
        let shared_text = shared.join("\n");
        assert!(shared_text.contains("by role") && shared_text.contains("planner"));
        for line in &shared {
            assert!(
                footer.contains(line.as_str()),
                "footer missing shared role line {line:?}:\n{footer}"
            );
            assert!(
                session.contains(line.as_str()),
                "session stats missing shared role line {line:?}:\n{session}"
            );
        }
    }

    /// Only format_role_split_lines owns the by-role planner/executor templates.
    #[test]
    fn by_role_templates_single_source() {
        let src = include_str!("session_stats.rs");
        // Production half: strip tests module.
        let prod = src
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("production session_stats");
        // Emitted line template (docs may mention "by role" in prose).
        let template_hits: Vec<_> = prod.match_indices("\"  by role\"").collect();
        assert_eq!(
            template_hits.len(),
            1,
            "exactly one emitted 'by role' line template in production session_stats (got {})",
            template_hits.len()
        );
        assert!(
            prod.contains("format_role_split_lines"),
            "role-split helper must exist"
        );
        // Both report builders must call the helper (not re-implement).
        let usage_fn = prod
            .split("pub fn format_usage_end")
            .nth(1)
            .expect("format_usage_end");
        let usage_body = usage_fn.split("pub fn ").next().unwrap_or(usage_fn);
        assert!(
            usage_body.contains("format_role_split_lines"),
            "format_usage_end must call format_role_split_lines"
        );
        let stats_fn = prod
            .split("pub fn format_session_stats")
            .nth(1)
            .expect("format_session_stats");
        let stats_body = stats_fn.split("pub fn ").next().unwrap_or(stats_fn);
        assert!(
            stats_body.contains("format_role_split_lines"),
            "format_session_stats must call format_role_split_lines"
        );
    }

    #[test]
    fn report_pins_from_cli_prefers_strategy_over_profile() {
        let p = ReportPins::from_cli(
            Some("strong".into()),
            Some("plan-exec".into()),
            Some("weak".into()),
        );
        assert_eq!(p.plan_model(), Some("strong"));
        assert_eq!(p.strategy(), Some("plan-exec"));
        let p2 = ReportPins::from_cli(Some("s".into()), None, Some("profile-x".into()));
        assert_eq!(p2.strategy(), Some("profile-x"));
    }

    #[test]
    fn absorb_tool_results() {
        let mut c = SessionClock::new();
        let msgs = vec![
            Message::user("hi"),
            Message::ToolResult(pirs_ai::ToolResultMessage {
                tool_call_id: "1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text("ok")],
                details: None,
                is_error: false,
                terminate: false,
                timestamp: 0,
            }),
            Message::ToolResult(pirs_ai::ToolResultMessage {
                tool_call_id: "2".into(),
                tool_name: "edit".into(),
                content: vec![ContentBlock::text("fail")],
                details: None,
                is_error: true,
                terminate: false,
                timestamp: 0,
            }),
        ];
        c.absorb_messages(&msgs);
        assert_eq!(c.tool_calls(), 2);
        assert_eq!(c.tool_errors(), 1);
    }
}
