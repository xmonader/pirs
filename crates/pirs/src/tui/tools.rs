use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::theme::Theme;

pub(crate) const TOOL_PREVIEW_CAP: usize = 200;
pub(crate) const TOOL_BODY_SHOW: usize = 12;

pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    } else {
        s
    }
}

/// Status glyph column (fixed width) — qwen-style state machine.
pub(crate) fn tool_status_glyph(done: bool, is_error: bool, tick: u64) -> (&'static str, Style) {
    // tick unused for static done glyphs; live spinner uses draw path.
    let _ = tick;
    if !done {
        ("○", Style::default().fg(Color::Rgb(56, 189, 248)))
    } else if is_error {
        ("✗", Style::default().fg(Color::Rgb(248, 113, 113)))
    } else {
        ("✓", Style::default().fg(Color::Rgb(74, 222, 128)))
    }
}

pub(crate) fn tool_verb(name: &str, done: bool) -> &'static str {
    match (name, done) {
        ("bash", false) => "Running",
        ("bash", true) => "Ran",
        ("read", _) => "Read",
        ("write", false) => "Writing",
        ("write", true) => "Wrote",
        ("edit" | "edit_block", false) => "Editing",
        ("edit" | "edit_block", true) => "Edited",
        ("grep" | "find", _) => "Searched",
        ("ls", _) => "Listed",
        ("run_tests", false) => "Testing",
        ("run_tests", true) => "Tested",
        ("delegate" | "run_subagent", false) => "Delegating",
        ("delegate" | "run_subagent", true) => "Delegated",
        ("web" | "web_fetch" | "web_search", _) => "Fetched",
        ("project", false) => "Project",
        ("project", true) => "Project",
        (_, false) => "Calling",
        (_, true) => "Called",
    }
}

/// Quiet tools collapse to a single header line on success.
pub(crate) fn tool_is_quiet(name: &str) -> bool {
    matches!(
        name,
        "read" | "grep" | "find" | "ls" | "recall" | "todo" | "audit"
    )
}

pub(crate) fn tool_default_expanded(name: &str, is_error: bool) -> bool {
    is_error || !tool_is_quiet(name)
}

pub(crate) fn tool_operand_style<'a>(theme: &'a Theme, name: &str) -> Style {
    match name {
        "bash" | "run_tests" | "project" => theme.command,
        "read" | "write" | "edit" | "edit_block" | "ls" | "grep" | "find" => theme.path,
        _ => theme.tool_args,
    }
}

pub(crate) fn render_tool_call(
    theme: &Theme,
    name: &str,
    summary: &str,
    preview: &str,
    is_error: bool,
    done: bool,
    expanded: bool,
) -> Vec<Line<'static>> {
    let (glyph, gstyle) = tool_status_glyph(done, is_error, 0);
    let verb = tool_verb(name, done);
    let mut spans = vec![
        Span::styled(format!("  {glyph} "), gstyle),
        Span::styled(format!("{verb} "), theme.tool_name),
        Span::styled(name.to_string(), theme.tool_name),
    ];
    if !summary.is_empty() {
        spans.push(Span::styled("  ", theme.dim));
        spans.push(Span::styled(
            truncate_chars(summary, 90),
            tool_operand_style(theme, name),
        ));
    }
    let mut out = vec![Line::from(spans)];

    let show_body = expanded && !preview.is_empty();
    if !show_body {
        if done && !preview.is_empty() && tool_is_quiet(name) && !is_error {
            // collapsed quiet tool — header only
            return out;
        }
        if !done {
            return out;
        }
        if preview.is_empty() {
            return out;
        }
        // Not expanded but has preview (shouldn't happen often): show overflow hint.
        let n = preview.lines().count();
        if n > 0 && !expanded {
            out.push(Line::from(Span::styled(
                format!("    ▶ +{n} lines  (tab expand)"),
                theme.dim,
            )));
            return out;
        }
    }

    if show_body {
        let default_style = if is_error {
            theme.tool_err
        } else {
            theme.tool_ok
        };
        let diffish = matches!(name, "edit" | "write" | "edit_block" | "apply_patch");
        let lines: Vec<&str> = preview.lines().collect();
        let total = lines.len();
        let show = lines.iter().take(TOOL_BODY_SHOW);
        let count = total.min(TOOL_BODY_SHOW);
        for (i, l) in show.enumerate() {
            let border = if i + 1 == count && total <= TOOL_BODY_SHOW {
                "⎣"
            } else {
                "⎢"
            };
            let style = if is_error {
                theme.tool_err
            } else if diffish {
                let t = l.trim_start();
                if t.starts_with('+') && !t.starts_with("+++") {
                    theme.success
                } else if t.starts_with('-') && !t.starts_with("---") {
                    theme.tool_err
                } else if t.starts_with("@@") {
                    theme.accent
                } else {
                    default_style
                }
            } else {
                default_style
            };
            out.push(Line::from(Span::styled(
                format!("  {border} {l}"),
                style,
            )));
        }
        if total > TOOL_BODY_SHOW {
            out.push(Line::from(Span::styled(
                format!("  ⎣ ▶ +{} lines", total - TOOL_BODY_SHOW),
                theme.dim,
            )));
        }
    }
    out
}

pub(crate) fn tool_group_label(name: &str, n: usize) -> String {
    match name {
        "read" => format!("Read {n} file{}", if n == 1 { "" } else { "s" }),
        "grep" | "find" => format!("Searched {n} time{}", if n == 1 { "" } else { "s" }),
        "ls" => format!("Listed {n} path{}", if n == 1 { "" } else { "s" }),
        _ => format!("{name} × {n}"),
    }
}

pub(crate) fn render_tool_group(
    theme: &Theme,
    name: &str,
    members: &[(String, bool)],
    expanded: bool,
) -> Vec<Line<'static>> {
    let any_err = members.iter().any(|(_, e)| *e);
    let (glyph, gstyle) = tool_status_glyph(true, any_err, 0);
    let label = tool_group_label(name, members.len());
    let mut out = vec![Line::from(vec![
        Span::styled(format!("  {glyph} "), gstyle),
        Span::styled(label, theme.tool_name),
        Span::styled(
            if expanded {
                "  ▼ tab collapse"
            } else {
                "  ▶ tab expand"
            },
            theme.dim,
        ),
    ])];
    if expanded {
        for (i, (summary, is_err)) in members.iter().enumerate() {
            let border = if i + 1 == members.len() { "⎣" } else { "⎢" };
            let style = if *is_err {
                theme.tool_err
            } else {
                tool_operand_style(theme, name)
            };
            out.push(Line::from(Span::styled(
                format!("  {border} {}", truncate_chars(summary, 90)),
                style,
            )));
        }
    }
    out
}

