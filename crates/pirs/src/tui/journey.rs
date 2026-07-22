use std::path::PathBuf;

use ratatui::text::{Line, Span};

use super::theme::{composer_mode_style, Theme};

/// Starter prompts for empty-input digit shortcuts (first-run friction cut).
pub(crate) const STARTER_PROMPTS: &[&str] = &[
    "Explain this repository: structure, how to build/test, and the top 3 things I should know.",
    "Run the project's tests (or the closest equivalent) and fix any failures you find.",
    "Review uncommitted git changes and summarize risks before I commit.",
];

pub(crate) const SESSION_TIPS: &[&str] = &[
    "Type a goal in plain English — Enter sends, alt+enter is newline",
    "While the agent works, type to steer · esc cancels",
    "Tab expands the last tool card · ctrl-o toggles thoughts",
    "/plan is read-only explore · /act enables writes",
    "Approvals: y = once · a = always this session · n = deny",
    "!cargo test runs a local shell command (records in context)",
    "/strategy plan-exec uses a planner then an executor",
    "Type / for slash commands — Tab completes, ↑↓ pick",
];

pub(crate) fn tip_for_session() -> &'static str {
    let day = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as usize;
    SESSION_TIPS[day % SESSION_TIPS.len()]
}

pub(crate) fn tui_onboard_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pirs").join("tui_onboarded")
}

pub(crate) fn is_first_tui_run() -> bool {
    if std::env::var("PIRS_TUI_FORCE_ONBOARD").is_ok() {
        return true;
    }
    if std::env::var("PIRS_TUI_SKIP_ONBOARD").is_ok() {
        return false;
    }
    !tui_onboard_path().is_file()
}

pub(crate) fn mark_tui_onboarded() {
    let path = tui_onboard_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, b"1\n");
}

pub(crate) fn render_welcome(
    theme: &Theme,
    model: &str,
    plan_model: Option<&str>,
    strategy: Option<&str>,
    approval: &str,
    cwd: &str,
    first_run: bool,
) -> Vec<Line<'static>> {
    let mut meta = vec![
        Span::styled(model.to_string(), theme.accent),
        Span::styled("  ·  ", theme.dim),
        Span::styled(
            format!("● {approval}"),
            composer_mode_style(theme, approval, false, false),
        ),
        Span::styled("  ·  ", theme.dim),
        Span::styled(format!("~/{cwd}"), theme.header_bg),
    ];
    if let Some(p) = plan_model {
        meta.push(Span::styled("  ·  plan:", theme.dim));
        meta.push(Span::styled(p.to_string(), theme.plan));
    }
    if let Some(s) = strategy {
        meta.push(Span::styled("  ·  ", theme.dim));
        meta.push(Span::styled(s.to_string(), theme.accent));
    }

    let mut out = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  pirs", theme.brand),
            Span::styled(
                if first_run {
                    "  ·  welcome — 60-second start"
                } else {
                    "  ·  agent console"
                },
                theme.dim,
            ),
        ]),
        Line::from({
            let mut spans = vec![Span::styled("  ", theme.dim)];
            spans.extend(meta);
            spans
        }),
    ];

    if first_run {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            "  Getting started",
            theme.heading,
        )));
        out.push(Line::from(Span::styled(
            "  1. Type a goal in plain English → Enter",
            theme.assistant_text,
        )));
        out.push(Line::from(Span::styled(
            "  2. Watch tools (✓ Read · Ran bash) — Tab expands details",
            theme.assistant_text,
        )));
        out.push(Line::from(Span::styled(
            "  3. If asked: y yes · a always · n no",
            theme.assistant_text,
        )));
        out.push(Line::from(Span::styled(
            "  4. ? keys anytime · / for commands · esc cancels a run",
            theme.assistant_text,
        )));
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            "  Starters — press 1 / 2 / 3 when the input is empty:",
            theme.heading,
        )));
        out.push(Line::from(Span::styled(
            "  1  Explain this repo",
            theme.plan,
        )));
        out.push(Line::from(Span::styled(
            "  2  Run tests & fix failures",
            theme.plan,
        )));
        out.push(Line::from(Span::styled(
            "  3  Review uncommitted changes",
            theme.plan,
        )));
        out.push(Line::from(Span::styled(
            "  (fills the input — press Enter when ready)",
            theme.placeholder,
        )));
    } else {
        out.push(Line::from(Span::styled(
            format!("  tip: {}", tip_for_session()),
            theme.placeholder,
        )));
        out.push(Line::from(Span::styled(
            "  starters 1–3 · /tour tour · ? help · type / for commands",
            theme.placeholder,
        )));
    }
    out.push(Line::from(""));
    out
}

