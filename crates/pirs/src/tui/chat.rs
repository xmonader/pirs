use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::journey::render_welcome;
use super::theme::Theme;
use super::tools::*;

// ── Structured chat ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(super) enum ChatItem {
    System(String),
    /// Rich welcome empty-state (rendered specially; one-shot at session start).
    Welcome {
        model: String,
        plan_model: Option<String>,
        strategy: Option<String>,
        approval: String,
        cwd: String,
        first_run: bool,
    },
    User(String),
    Assistant {
        thinking: String,
        text: String,
        error: Option<String>,
    },
    /// Unified tool call card (running → done). Prefer updating in place on end.
    ToolCall {
        name: String,
        summary: String,
        preview: String,
        is_error: bool,
        done: bool,
        expanded: bool,
    },
    /// Collapsed run of quiet tools (e.g. "Read 3 files") — grok verb-group pattern.
    ToolGroup {
        name: String,
        /// (summary/operand, is_error)
        members: Vec<(String, bool)>,
        expanded: bool,
    },
    Notice(String),
}

impl ChatItem {
    pub(super) fn render(&self, theme: &Theme, width: usize, thinking_expanded: bool) -> Vec<Line<'static>> {
        match self {
            ChatItem::System(text) => {
                let mut out: Vec<Line<'static>> = text
                    .lines()
                    .map(|l| {
                        Line::from(Span::styled(format!("    {l}"), theme.system))
                    })
                    .collect();
                out.push(Line::from(""));
                out
            }
            ChatItem::Welcome {
                model,
                plan_model,
                strategy,
                approval,
                cwd,
                first_run,
            } => render_welcome(
                theme,
                model,
                plan_model.as_deref(),
                strategy.as_deref(),
                approval,
                cwd,
                *first_run,
            ),
            ChatItem::User(text) => {
                // Extra blank above/below so turns don't stack edge-to-edge.
                let mut out = vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("  │ ", theme.user_label),
                        Span::styled("you", theme.user_label),
                    ]),
                ];
                for l in text.lines() {
                    out.push(Line::from(vec![
                        Span::styled("  │ ", theme.user_label),
                        Span::styled(l.to_string(), theme.user_text),
                    ]));
                }
                out.push(Line::from(""));
                out
            }
            ChatItem::Assistant {
                thinking,
                text,
                error,
            } => {
                let mut out = vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("  │ ", theme.assistant_label),
                        Span::styled("assistant", theme.assistant_label),
                    ]),
                ];
                if !thinking.trim().is_empty() {
                    out.extend(render_thinking(thinking, theme, thinking_expanded));
                }
                if !text.trim().is_empty() {
                    for line in render_markdown(text, theme, width.saturating_sub(4)) {
                        out.push(line);
                    }
                }
                if let Some(err) = error {
                    out.push(Line::from(Span::styled(
                        format!("    ⚠ {err}"),
                        theme.error,
                    )));
                }
                out.push(Line::from(""));
                out
            }
            ChatItem::ToolCall {
                name,
                summary,
                preview,
                is_error,
                done,
                expanded,
            } => render_tool_call(theme, name, summary, preview, *is_error, *done, *expanded),
            ChatItem::ToolGroup {
                name,
                members,
                expanded,
            } => render_tool_group(theme, name, members, *expanded),
            ChatItem::Notice(text) => vec![
                Line::from(Span::styled(format!("    · {text}"), theme.system)),
                Line::from(""),
            ],
        }
    }
}

/// Live thinking while the model is generating.
///
/// Collapsed (default): one **stable** status line (count only — no streaming
/// hint text). A changing tail hint rewrote the end of the line every token
/// and made the caret look like it was jumping start↔end.
/// Expanded (`t` / ctrl-o): last few lines of reasoning.
pub(super) fn render_thinking_live(
    thinking: &str,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = thinking.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    if total == 0 {
        return Vec::new();
    }
    if !expanded {
        // Fixed shape: only the integer changes. No trailing live snippet.
        return vec![Line::from(vec![
            Span::styled("    · ", theme.accent),
            Span::styled("thinking", theme.thinking),
            Span::styled(
                format!(
                    " · {total} line{} · t expand",
                    if total == 1 { "" } else { "s" }
                ),
                theme.dim,
            ),
        ])];
    }
    const TAIL: usize = 6;
    let skip = total.saturating_sub(TAIL);
    let mut out = vec![Line::from(vec![
        Span::styled("    · ", theme.accent),
        Span::styled(
            format!("thinking · {total} lines · t collapse"),
            theme.thinking,
        ),
    ])];
    if skip > 0 {
        out.push(Line::from(Span::styled(
            format!("      … +{skip} earlier"),
            theme.dim,
        )));
    }
    for l in lines.into_iter().skip(skip) {
        out.push(Line::from(Span::styled(
            format!("      {l}"),
            theme.thinking,
        )));
    }
    out
}

pub(super) fn line_word(n: usize) -> &'static str {
    if n == 1 {
        "line"
    } else {
        "lines"
    }
}

pub(super) fn render_thinking(thinking: &str, theme: &Theme, expanded: bool) -> Vec<Line<'static>> {
    // History view: generous window when expanded.
    const MAX: usize = 80;
    let lines: Vec<&str> = thinking.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    if total == 0 {
        return Vec::new();
    }
    if !expanded {
        // Compact chip — no wall of chrome for a single reasoning line.
        return vec![Line::from(vec![
            Span::styled("    · ", theme.dim),
            Span::styled(
                format!("thought · {total} {} · t", line_word(total)),
                theme.thinking,
            ),
        ])];
    }
    let skip = total.saturating_sub(MAX);
    let mut out = vec![Line::from(vec![
        Span::styled("    · ", theme.dim),
        Span::styled(
            format!("thought · {total} {} · t hide", line_word(total)),
            theme.thinking,
        ),
    ])];
    if skip > 0 {
        out.push(Line::from(Span::styled(
            format!("      … {skip} earlier omitted"),
            theme.dim,
        )));
    }
    for l in lines.into_iter().skip(skip) {
        out.push(Line::from(Span::styled(
            format!("      {l}"),
            theme.thinking,
        )));
    }
    out
}

/// Lightweight markdown → ratatui lines. Handles headings, fenced code,
/// bullets, and inline `code` / **bold**. Not a full parser — enough for
/// typical assistant replies without dragging in a crate.
pub(super) fn render_markdown(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();

    for raw in text.lines() {
        let line = raw;
        // Content indent: 4 cols so chat breathes vs chrome.
        const IND: &str = "    ";
        if let Some(rest) = line.strip_prefix("```") {
            if in_code {
                in_code = false;
                code_lang.clear();
                out.push(Line::from(Span::styled(format!("{IND}╰──"), theme.dim)));
            } else {
                in_code = true;
                code_lang = rest.trim().to_string();
                let label = if code_lang.is_empty() {
                    "code".to_string()
                } else {
                    code_lang.clone()
                };
                out.push(Line::from(Span::styled(
                    format!("{IND}╭─ {label}"),
                    theme.dim,
                )));
            }
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                format!("{IND}│ {line}"),
                theme.code_block,
            )));
            continue;
        }

        if let Some(rest) = line.strip_prefix("### ") {
            out.push(Line::from(Span::styled(
                format!("{IND}{rest}"),
                theme.heading,
            )));
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            out.push(Line::from(Span::styled(
                format!("{IND}{rest}"),
                theme.heading,
            )));
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            out.push(Line::from(Span::styled(
                format!("{IND}{rest}"),
                theme.heading.add_modifier(Modifier::UNDERLINED),
            )));
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let mut spans = vec![Span::styled(format!("{IND}• "), theme.accent)];
            spans.extend(inline_spans(rest, theme));
            out.push(Line::from(spans));
            continue;
        }
        if line.is_empty() {
            out.push(Line::from(""));
            continue;
        }

        // Soft-wrap long plain lines at word boundaries for readability.
        let content_w = width.max(20);
        let ind_w = 4usize;
        let rendered = inline_spans(line, theme);
        let plain_w: usize = rendered
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if plain_w + ind_w <= content_w {
            let mut spans = vec![Span::raw(IND.to_string())];
            spans.extend(rendered);
            out.push(Line::from(spans));
        } else {
            for chunk in wrap_words(line, content_w.saturating_sub(ind_w)) {
                let mut spans = vec![Span::raw(IND.to_string())];
                spans.extend(inline_spans(&chunk, theme));
                out.push(Line::from(spans));
            }
        }
    }
    out
}

pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let wlen = unicode_width::UnicodeWidthStr::width(word);
        let cur_w = unicode_width::UnicodeWidthStr::width(cur.as_str());
        if cur.is_empty() {
            if wlen > width {
                // Hard-split overlong tokens.
                let mut buf = String::new();
                for ch in word.chars() {
                    let cw = unicode_width::UnicodeWidthStr::width(buf.as_str());
                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    if cw + ch_w > width && !buf.is_empty() {
                        lines.push(std::mem::take(&mut buf));
                    }
                    buf.push(ch);
                }
                if !buf.is_empty() {
                    cur = buf;
                }
            } else {
                cur.push_str(word);
            }
        } else if cur_w + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Parse inline `code` and **bold** spans.
pub(super) fn inline_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), style));
        }
    };

    while i < chars.len() {
        // **bold**
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_closing(&chars, i + 2, &['*', '*']) {
                flush(&mut buf, &mut spans, theme.assistant_text);
                let inner: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(inner, theme.bold));
                i = end + 2;
                continue;
            }
        }
        // `code`
        if chars[i] == '`' {
            if let Some(end) = find_closing(&chars, i + 1, &['`']) {
                flush(&mut buf, &mut spans, theme.assistant_text);
                let inner: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(inner, theme.code));
                i = end + 1;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut spans, theme.assistant_text);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), theme.assistant_text));
    }
    spans
}

pub(super) fn find_closing(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
    let n = needle.len();
    let mut i = start;
    while i + n <= chars.len() {
        if chars[i..i + n] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

