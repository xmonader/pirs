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
    lines.push(format!(
        "  wall time      {}",
        format_duration(wall)
    ));
    lines.push(format!(
        "  agent time     {}  (idle {})",
        format_duration(agent),
        format_duration(idle)
    ));
    lines.push(format!(
        "  user turns     {}",
        clock.user_turns()
    ));
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
        lines.push(format!("  est. cost      ${cost_total:.4}  (builtin price table)"));
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
    if !report.by_model.is_empty() {
        lines.push("  by model".into());
        for (m, u) in &report.by_model {
            let calls = report.calls.iter().filter(|c| {
                c.model == *m || c.model == format!("delegate:{m}")
            }).count();
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

/// Print session stats to stderr (safe after TUI restores the terminal).
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
        *report.by_model.entry("deepseek-v4-flash".into()).or_default() = Usage {
            input: 1000,
            output: 200,
            cache_read: 100,
            ..Default::default()
        };
        let s = format_session_stats(&clock, &report, "qwen3.5-plus", Some("deepseek-v4-pro"), Some("plan-exec"));
        assert!(s.contains("session stats"));
        assert!(s.contains("wall time"));
        assert!(s.contains("agent time"));
        assert!(s.contains("tokens"));
        assert!(s.contains("deepseek-v4-flash"));
        assert!(s.contains("plan-exec"));
        assert!(s.contains("qwen3.5-plus"));
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
