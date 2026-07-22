//! Interactive terminal UI for pirs (`--mode tui`).
//!
//! Layout (top → bottom), polished against grok-build / mistral-vibe / qwen-code:
//!   header      — brand · model · plan/strat · approval · cwd
//!   chat        — role accents, collapsible tools/thinking, markdown
//!   turn-status — activity spinner · elapsed · usage · cancel/help
//!   input       — mode-colored composer (plan / yolo / steer / approval)
//!
//! Theme: `PIRS_TUI_THEME=mono` for dim/bold-only terminals.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use crossterm::ExecutableCommand;
use pirs_agent::{Agent, AgentEvent, AgentTool};
use pirs_ai::Message;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;

use crate::approval::ApprovalMode;
use crate::session_stats::{self, SessionClock};

mod journey;
mod slash;
mod theme;
mod tools;

use journey::*;
use slash::*;
use theme::*;
use tools::*;

// ── Structured chat ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum ChatItem {
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
    fn render(&self, theme: &Theme, width: usize, thinking_expanded: bool) -> Vec<Line<'static>> {
        match self {
            ChatItem::System(text) => text
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), theme.system)))
                .collect(),
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
                let mut out = vec![Line::from(vec![
                    Span::styled("│ ", theme.user_label),
                    Span::styled("you", theme.user_label),
                ])];
                for l in text.lines() {
                    out.push(Line::from(vec![
                        Span::styled("│ ", theme.user_label),
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
                let mut out = vec![Line::from(vec![
                    Span::styled("│ ", theme.assistant_label),
                    Span::styled("assistant", theme.assistant_label),
                ])];
                if !thinking.trim().is_empty() {
                    out.extend(render_thinking(thinking, theme, thinking_expanded));
                }
                if !text.trim().is_empty() {
                    for line in render_markdown(text, theme, width.saturating_sub(2)) {
                        // Prefix accent on plain content lines that already have "  " indent.
                        out.push(line);
                    }
                }
                if let Some(err) = error {
                    out.push(Line::from(Span::styled(format!("  ⚠ {err}"), theme.error)));
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
            ChatItem::Notice(text) => vec![Line::from(Span::styled(
                format!("  · {text}"),
                theme.system,
            ))],
        }
    }
}

fn render_thinking(thinking: &str, theme: &Theme, expanded: bool) -> Vec<Line<'static>> {
    const MAX: usize = 8;
    let lines: Vec<&str> = thinking.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    if total == 0 {
        return Vec::new();
    }
    if !expanded {
        return vec![Line::from(Span::styled(
            format!("  ▶ thought · {total} line{}", if total == 1 { "" } else { "s" }),
            theme.thinking,
        ))];
    }
    let skip = total.saturating_sub(MAX);
    let mut out = vec![Line::from(Span::styled(
        format!("  ▼ thought · {total} lines  (ctrl-o collapse)"),
        theme.thinking,
    ))];
    for l in lines.into_iter().skip(skip) {
        out.push(Line::from(Span::styled(
            format!("    {l}"),
            theme.thinking,
        )));
    }
    out
}

/// Lightweight markdown → ratatui lines. Handles headings, fenced code,
/// bullets, and inline `code` / **bold**. Not a full parser — enough for
/// typical assistant replies without dragging in a crate.
fn render_markdown(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();

    for raw in text.lines() {
        let line = raw;
        if let Some(rest) = line.strip_prefix("```") {
            if in_code {
                in_code = false;
                code_lang.clear();
                out.push(Line::from(Span::styled("  ╰──", theme.dim)));
            } else {
                in_code = true;
                code_lang = rest.trim().to_string();
                let label = if code_lang.is_empty() {
                    "code".to_string()
                } else {
                    code_lang.clone()
                };
                out.push(Line::from(Span::styled(format!("  ╭─ {label}"), theme.dim)));
            }
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                format!("  │ {line}"),
                theme.code_block,
            )));
            continue;
        }

        if let Some(rest) = line.strip_prefix("### ") {
            out.push(Line::from(Span::styled(format!("  {rest}"), theme.heading)));
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            out.push(Line::from(Span::styled(format!("  {rest}"), theme.heading)));
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            out.push(Line::from(Span::styled(
                format!("  {rest}"),
                theme.heading.add_modifier(Modifier::UNDERLINED),
            )));
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let mut spans = vec![Span::styled("  • ", theme.accent)];
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
        let rendered = inline_spans(line, theme);
        let plain_w: usize = rendered
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if plain_w + 2 <= content_w {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(rendered);
            out.push(Line::from(spans));
        } else {
            // Fall back to character wrap on the plain string with styles reapplied simply.
            for chunk in wrap_words(line, content_w.saturating_sub(2)) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(inline_spans(&chunk, theme));
                out.push(Line::from(spans));
            }
        }
    }
    out
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
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
fn inline_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
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

fn find_closing(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
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

// ── App state ───────────────────────────────────────────────────────────────

/// One committed chat item's wrapped-row cache. `rows: None` means the exact
/// rows aren't currently known at the active width — either the item was
/// just pushed and has never been measured, or its previous measurement was
/// invalidated by a resize, or it was evicted for being far off-screen — in
/// all three cases `row_count` still holds a best-known estimate (possibly
/// stale) so scroll-position math stays correct without needing the exact
/// content.
struct ItemCache {
    rows: Option<Vec<Line<'static>>>,
    row_count: usize,
}

pub struct TuiOptions {
    pub agent: Agent,
    pub host: Option<Arc<pirs_rhai::ExtensionHost>>,
    #[allow(dead_code)]
    pub session_path: std::path::PathBuf,
    pub approval_mode: ApprovalMode,
    pub approval_gate: Option<Arc<crate::approval::ApprovalGate>>,
    pub cwd: std::path::PathBuf,
    /// Initial strategy name (e.g. plan-exec); changeable via `/strategy`.
    pub strategy: Option<String>,
    /// Initial plan-model; changeable via `/plan-model`.
    pub plan_model: Option<String>,
    pub verify: Option<String>,
    pub max_attempts: Option<u32>,
    /// Full tool set for strategy phase scoping.
    pub strategy_tools: Vec<Arc<dyn AgentTool>>,
    pub recorder: Option<Arc<pirs_agent::trace::Recorder>>,
    pub trace_phase: Option<Arc<Mutex<String>>>,
    /// Registry aliases for `/model` listing.
    pub model_aliases: Vec<String>,
}

/// Live session controls shared between the UI thread and the agent worker.
#[derive(Clone, Default)]
struct SessionControls {
    strategy: Option<String>,
    plan_model: Option<String>,
}

struct App {
    items: Vec<ChatItem>,
    /// Live streaming assistant content (thinking + text), not yet committed.
    live: Option<(String, String)>,
    input: String,
    /// Byte index of the cursor inside `input`.
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    history_draft: String,
    running: bool,
    tick: u64,
    dirty: bool,
    last_live_refresh: std::time::Instant,
    steer_queue: Arc<Mutex<Vec<String>>>,
    /// Rows scrolled up from the bottom (0 = pinned).
    scroll: u16,
    viewport_height: u16,
    model: String,
    plan_model: Option<String>,
    strategy: Option<String>,
    model_aliases: Vec<String>,
    approval_mode: String,
    cwd: PathBuf,
    cwd_label: String,
    usage_summary: String,
    pending_approval: Arc<Mutex<Option<String>>>,
    approval_answer: Arc<std::sync::mpsc::Sender<String>>,
    /// When the current approval prompt was shown (grace period for Enter).
    approval_opened_at: Option<std::time::Instant>,
    cancel: pirs_agent::agent::CancelSlot,
    show_help: bool,
    status_msg: String,
    /// Human activity label for turn-status ("thinking", "bash", …).
    last_activity: String,
    turn_started_at: Option<std::time::Instant>,
    thinking_expanded: bool,
    /// Selected row in the slash completion popup (0-based into filtered list).
    slash_sel: usize,
    /// Session started as first-run onboarding (for /tour re-show).
    first_run_session: bool,
    should_quit: bool,
    /// One entry per `items[i]`: wrapped physical rows for that item, when
    /// known — virtualized so a long conversation with large tool outputs
    /// doesn't pay to re-wrap the *entire* history every time a single new
    /// item is pushed. See `ItemCache` and `draw_chat`'s three-pass
    /// measure/clamp/paint for how entries near the viewport stay exactly
    /// measured while everything else keeps only a row-count estimate.
    item_caches: Vec<ItemCache>,
    cache_width: usize,
    total_rows: usize,
    last_draw_width: usize,
    /// Where the input-box cursor should sit, computed fresh by `draw_input`
    /// on every render. Read by the custom draw wrapper (see `draw_dedup_cursor`)
    /// so the actual terminal cursor escape is only re-emitted when this
    /// value changes between frames — `ratatui::Terminal::draw`/`try_draw`
    /// unconditionally re-sends Show+MoveTo on *every* call regardless of
    /// whether the position changed, which resets the terminal's cursor
    /// blink phase on every streamed token. Confirmed unfixed as of ratatui
    /// 0.30/ratatui-core 0.1.2 (`apply_buffer_with_cursor` has no dedup),
    /// so this is handled at the application level instead of waiting on
    /// upstream.
    desired_cursor: Option<(u16, u16)>,
    /// Session wall / agent-busy timers for exit stats.
    clock: SessionClock,
}

impl App {
    fn push(&mut self, item: ChatItem) {
        self.items.push(item);
        self.dirty = true;
        // Unmeasured placeholder: App::push has no `theme`/width to render
        // with, so the real row count is filled in lazily by draw_chat, the
        // first time this item is (or might be) actually painted.
        self.item_caches.push(ItemCache {
            rows: None,
            row_count: 1,
        });
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.push(ChatItem::Notice(text.into()));
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = msg.into();
        self.dirty = true;
    }

    /// Clear transcript + caches (Ctrl-L / `/clear`). Keeps length invariant.
    fn clear_chat(&mut self) {
        self.items.clear();
        self.item_caches.clear();
        self.live = None;
        self.scroll = 0;
        self.total_rows = 0;
        self.cache_width = 0;
        self.notice("screen cleared");
    }

    fn invalidate_item(&mut self, idx: usize) {
        if let Some(c) = self.item_caches.get_mut(idx) {
            c.rows = None;
            c.row_count = 1;
        }
        self.dirty = true;
    }

    fn start_tool(&mut self, name: String, summary: String) {
        self.last_activity = name.clone();
        self.push(ChatItem::ToolCall {
            name,
            summary,
            preview: String::new(),
            is_error: false,
            done: false,
            expanded: false,
        });
    }

    fn finish_tool(&mut self, name: &str, preview: String, is_error: bool) {
        let expanded = tool_default_expanded(name, is_error);
        let mut finished = false;
        for i in (0..self.items.len()).rev() {
            if let ChatItem::ToolCall {
                name: n,
                done,
                preview: p,
                is_error: err,
                expanded: exp,
                ..
            } = &mut self.items[i]
            {
                if !*done && (n == name || name.is_empty()) {
                    *done = true;
                    *p = preview.clone();
                    *err = is_error;
                    *exp = expanded;
                    self.invalidate_item(i);
                    finished = true;
                    break;
                }
            }
        }
        if !finished {
            // No open card (e.g. shell) — push a finished one.
            self.push(ChatItem::ToolCall {
                name: if name.is_empty() {
                    "bash".into()
                } else {
                    name.into()
                },
                summary: String::new(),
                preview,
                is_error,
                done: true,
                expanded,
            });
        }
        self.collapse_trailing_quiet_tools();
    }

    /// Fold consecutive quiet success tools into a ToolGroup (Read 3 files).
    /// Also merges a new quiet tool into an immediately preceding same-name group.
    fn collapse_trailing_quiet_tools(&mut self) {
        let end = self.items.len();
        if end == 0 {
            return;
        }
        // Don't fold while a tool is still running at the end.
        if let Some(ChatItem::ToolCall { done: false, .. }) = self.items.last() {
            return;
        }

        // Collect trailing finished quiet success ToolCalls of one name.
        let mut start = end;
        let mut group_name: Option<String> = None;
        while start > 0 {
            match &self.items[start - 1] {
                ChatItem::ToolCall {
                    name,
                    done: true,
                    is_error: false,
                    ..
                } if tool_is_quiet(name) => {
                    if let Some(ref g) = group_name {
                        if g != name {
                            break;
                        }
                    } else {
                        group_name = Some(name.clone());
                    }
                    start -= 1;
                }
                _ => break,
            }
        }
        let call_count = end - start;
        if call_count == 0 {
            return;
        }
        let name = group_name.unwrap();

        // If the item before the run is already a same-name group, merge into it.
        if start > 0 {
            let can_merge = matches!(
                &self.items[start - 1],
                ChatItem::ToolGroup { name: gname, .. } if gname == &name
            );
            if can_merge {
                let mut extra = Vec::new();
                for item in &self.items[start..end] {
                    if let ChatItem::ToolCall {
                        summary, is_error, ..
                    } = item
                    {
                        extra.push((summary.clone(), *is_error));
                    }
                }
                if let ChatItem::ToolGroup { members, .. } = &mut self.items[start - 1] {
                    members.extend(extra);
                }
                self.items.drain(start..end);
                self.item_caches.drain(start..end);
                self.invalidate_item(start - 1);
                return;
            }
        }

        if call_count < 2 {
            return;
        }
        let mut members = Vec::with_capacity(call_count);
        for item in &self.items[start..end] {
            if let ChatItem::ToolCall {
                summary, is_error, ..
            } = item
            {
                members.push((summary.clone(), *is_error));
            }
        }
        self.items.drain(start..end);
        self.item_caches.drain(start..end);
        self.items.insert(
            start,
            ChatItem::ToolGroup {
                name,
                members,
                expanded: false,
            },
        );
        self.item_caches.insert(
            start,
            ItemCache {
                rows: None,
                row_count: 1,
            },
        );
        self.dirty = true;
    }

    fn toggle_last_tool_expand(&mut self) {
        for i in (0..self.items.len()).rev() {
            match &mut self.items[i] {
                ChatItem::ToolGroup { expanded, .. } => {
                    *expanded = !*expanded;
                    self.invalidate_item(i);
                    return;
                }
                ChatItem::ToolCall {
                    expanded,
                    preview,
                    done,
                    ..
                } if *done && !preview.is_empty() => {
                    *expanded = !*expanded;
                    self.invalidate_item(i);
                    return;
                }
                _ => {}
            }
        }
    }

    fn apply_starter(&mut self, idx: usize) {
        if let Some(p) = STARTER_PROMPTS.get(idx) {
            self.input = (*p).to_string();
            self.cursor = self.input.len();
            self.history_idx = None;
            self.dirty = true;
            self.set_status(format!("starter {} — press Enter to send", idx + 1));
        }
    }

    fn push_tour_welcome(&mut self) {
        self.push(ChatItem::Welcome {
            model: self.model.clone(),
            plan_model: self.plan_model.clone(),
            strategy: self.strategy.clone(),
            approval: self.approval_mode.clone(),
            cwd: self.cwd_label.clone(),
            first_run: true,
        });
    }

    fn mark_running(&mut self, activity: impl Into<String>) {
        self.running = true;
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(std::time::Instant::now());
        }
        self.last_activity = activity.into();
        self.dirty = true;
    }

    fn mark_idle(&mut self) {
        self.running = false;
        self.turn_started_at = None;
        self.last_activity.clear();
        self.set_status(String::new());
    }
}

// ── Terminal lifecycle ──────────────────────────────────────────────────────

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// Puts the real terminal into raw/alt-screen/mouse-capture mode, but the
/// returned `Terminal` renders into an in-memory buffer, not real stdout —
/// see `TuiWriter` for why. Terminal-size and cursor-position queries still
/// hit the real tty regardless of what the backend's writer is: crossterm's
/// `size()`/`cursor::position()` are separate ioctls, not routed through the
/// `Write` the backend wraps.
fn setup_terminal() -> anyhow::Result<Terminal<ratatui::backend::CrosstermBackend<Vec<u8>>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(crossterm::terminal::EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    let backend = ratatui::backend::CrosstermBackend::new(Vec::new());
    Ok(Terminal::new(backend)?)
}

/// A single-slot mailbox that always holds only the most recently pushed
/// value: `push` replaces — never queues behind — anything not yet taken.
/// This is the backpressure gate itself, factored out from `TuiWriter` so
/// its coalescing behavior is unit-testable without a real terminal or
/// background thread.
struct LatestSlot<T> {
    state: Mutex<LatestSlotState<T>>,
    cvar: std::sync::Condvar,
}

struct LatestSlotState<T> {
    value: Option<T>,
    closed: bool,
}

impl<T> LatestSlot<T> {
    fn new() -> Self {
        LatestSlot {
            state: Mutex::new(LatestSlotState {
                value: None,
                closed: false,
            }),
            cvar: std::sync::Condvar::new(),
        }
    }

    fn push(&self, value: T) {
        let mut guard = self.state.lock().unwrap();
        guard.value = Some(value);
        self.cvar.notify_one();
    }

    /// Blocks until a value is available, returning it immediately if one is
    /// already pending. Returns `None` once `close` has been called and
    /// nothing is left to take — the signal for the consumer to stop.
    fn take_blocking(&self) -> Option<T> {
        let mut guard = self.state.lock().unwrap();
        while guard.value.is_none() && !guard.closed {
            guard = self.cvar.wait(guard).unwrap();
        }
        guard.value.take()
    }

    fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        self.cvar.notify_one();
    }
}

/// Decouples the actual terminal write (a blocking OS syscall that can stall
/// under a slow pty/tmux/SSH pipe) from the async event loop that computes
/// frames. The loop renders each frame into an in-memory
/// `CrosstermBackend<Vec<u8>>` (cheap, CPU-only) and hands the resulting
/// bytes to this writer, which owns real stdout on a dedicated OS thread.
/// Only the LATEST pending frame is kept (via `LatestSlot`): if the writer
/// thread is still flushing a previous frame when a new one is computed,
/// the stale one is replaced rather than queued, so heavy token streaming
/// (many redraws in quick succession) never backs up waiting on terminal
/// I/O — the screen just catches up to the latest state once the writer is
/// free, which is all a human watching the screen can perceive anyway.
struct TuiWriter {
    slot: Arc<LatestSlot<Vec<u8>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TuiWriter {
    fn spawn() -> Self {
        let slot = Arc::new(LatestSlot::<Vec<u8>>::new());
        let worker_slot = Arc::clone(&slot);
        let handle = std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            while let Some(bytes) = worker_slot.take_blocking() {
                let _ = std::io::Write::write_all(&mut stdout, &bytes);
                let _ = std::io::Write::flush(&mut stdout);
            }
        });
        TuiWriter {
            slot,
            handle: Some(handle),
        }
    }

    /// Hands off a rendered frame's bytes to the writer thread, replacing —
    /// not queuing behind — any not-yet-written previous frame. Never
    /// blocks: this is the backpressure gate, expressed as "keep only the
    /// newest thing", not as flow control on the sender.
    fn push(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.slot.push(bytes);
        }
    }

    /// Signals shutdown and blocks until the writer thread has flushed
    /// whatever frame it was last given, so the final on-screen state (and
    /// any terminal-restore escape sequences written afterward) aren't
    /// racing an in-flight write on another thread. `Drop` calls this same
    /// logic (harmlessly, a second time) as a safety net for any early
    /// return between construction and the explicit call — otherwise an
    /// error propagating out of the loop would leak the writer thread,
    /// parked forever waiting on a signal nobody sends.
    fn shutdown(mut self) {
        self.close_and_join();
    }

    fn close_and_join(&mut self) {
        self.slot.close();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TuiWriter {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

fn restore_terminal() -> anyhow::Result<()> {
    let _ = std::io::stdout().execute(DisableMouseCapture);
    let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        prev(info);
    }));
}

// ── Entry ───────────────────────────────────────────────────────────────────

pub async fn run(mut opts: TuiOptions) -> anyhow::Result<()> {
    install_panic_hook();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    opts.agent.subscribe(Arc::new(move |event: AgentEvent| {
        let _ = event_tx.send(event);
    }));

    let (pending_approval, approval_answer_rx) = approval_bridge(&mut opts);

    let steer_sender = opts.agent.steer_sender();
    let steer_queue: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let steer_queue = Arc::clone(&steer_queue);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let msgs: Vec<String> = steer_queue.lock().unwrap().drain(..).collect();
            for m in msgs {
                steer_sender(Message::user(m));
            }
        });
    }

    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let tui_writer = TuiWriter::spawn();

    let model = opts.agent.model.clone();
    let approval_name = opts.approval_mode.name().to_string();
    let cwd_label = opts
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let cancel = opts.agent.cancel_handle();
    let controls = Arc::new(Mutex::new(SessionControls {
        strategy: opts.strategy.clone(),
        plan_model: opts.plan_model.clone(),
    }));

    let mut app = App {
        items: Vec::new(),
        live: None,
        input: String::new(),
        cursor: 0,
        history: Vec::new(),
        history_idx: None,
        history_draft: String::new(),
        running: false,
        tick: 0,
        dirty: true,
        last_live_refresh: std::time::Instant::now(),
        steer_queue,
        scroll: 0,
        viewport_height: 10,
        model,
        plan_model: opts.plan_model.clone(),
        strategy: opts.strategy.clone(),
        model_aliases: opts.model_aliases.clone(),
        approval_mode: approval_name,
        cwd: opts.cwd.clone(),
        cwd_label,
        usage_summary: String::new(),
        pending_approval,
        approval_answer: approval_answer_rx,
        approval_opened_at: None,
        cancel,
        show_help: false,
        status_msg: String::new(),
        last_activity: String::new(),
        turn_started_at: None,
        thinking_expanded: false,
        slash_sel: 0,
        first_run_session: is_first_tui_run(),
        should_quit: false,
        item_caches: Vec::new(),
        cache_width: 0,
        total_rows: 0,
        last_draw_width: 0,
        desired_cursor: None,
        clock: SessionClock::new(),
    };

    let first = app.first_run_session;
    app.push(ChatItem::Welcome {
        model: app.model.clone(),
        plan_model: app.plan_model.clone(),
        strategy: app.strategy.clone(),
        approval: app.approval_mode.clone(),
        cwd: app.cwd_label.clone(),
        first_run: first,
    });
    if first {
        mark_tui_onboarded();
    }

    let (prompt_tx, prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    // (command, output, record_in_agent_context)
    let (shell_tx, mut shell_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, String, bool)>();
    let agent = Arc::new(tokio::sync::Mutex::new(opts.agent));
    {
        // Strategy runner is `!Send` (Rc in gate/phases). Drive the agent on a
        // dedicated current-thread runtime so both plain prompts and strategies work.
        let agent = Arc::clone(&agent);
        let done_tx = done_tx.clone();
        let controls = Arc::clone(&controls);
        let strategy_tools = opts.strategy_tools.clone();
        let cwd = opts.cwd.clone();
        let verify = opts.verify.clone();
        let max_attempts = opts.max_attempts;
        let recorder = opts.recorder.clone();
        let trace_phase = opts.trace_phase.clone();
        std::thread::Builder::new()
            .name("pirs-tui-agent".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tui agent runtime");
                rt.block_on(async move {
                    let mut prompt_rx = prompt_rx;
                    while let Some(text) = prompt_rx.recv().await {
                        let (strategy, plan_model) = {
                            let c = controls.lock().unwrap();
                            (c.strategy.clone(), c.plan_model.clone())
                        };
                        let ok = if let Some(strat) = strategy {
                            let a = agent.lock().await;
                            let model = a.model.clone();
                            let result = crate::run_strategy_turn(
                                &a,
                                &text,
                                Some(strat.as_str()),
                                None,
                                &model,
                                plan_model.as_deref(),
                                strategy_tools.clone(),
                                &cwd,
                                verify.as_deref(),
                                max_attempts,
                                recorder.as_ref(),
                                trace_phase.clone(),
                            )
                            .await;
                            drop(a);
                            result.is_ok()
                        } else {
                            let mut a = agent.lock().await;
                            a.prompt(&text).await.is_ok()
                        };
                        let _ = done_tx.send(ok);
                    }
                });
            })
            .expect("spawn tui agent thread");
    }

    // Shared handles for slash commands (model / plan-model / strategy).
    let agent_for_cmds = Arc::clone(&agent);
    let controls_for_cmds = Arc::clone(&controls);

    let mut events = crossterm::event::EventStream::new();
    let mut last_cursor: Option<(u16, u16)> = None;
    loop {
        while let Ok(event) = event_rx.try_recv() {
            apply_agent_event(&mut app, event);
        }
        // Detect approval gate open for grace period + overlay (prompter is
        // off-thread; it only sets pending_approval).
        {
            let pending = app.pending_approval.lock().unwrap().is_some();
            if pending && app.approval_opened_at.is_none() {
                app.approval_opened_at = Some(std::time::Instant::now());
                app.dirty = true;
            } else if !pending && app.approval_opened_at.is_some() {
                app.approval_opened_at = None;
            }
        }
        while let Ok(ok) = done_rx.try_recv() {
            app.mark_idle();
            app.clock.agent_end();
            app.dirty = true;
            let report = {
                let a = agent.lock().await;
                a.usage_report()
            };
            let total = report.grand_total();
            let hit = if total.input + total.cache_read > 0 {
                100.0 * total.cache_read as f64 / (total.input + total.cache_read) as f64
            } else {
                0.0
            };
            app.usage_summary = format_tokens(total.input, total.output, hit);
            if !ok {
                app.push(ChatItem::Notice("run failed".into()));
            }
            app.set_status(String::new());
        }
        while let Ok((cmd, output, record)) = shell_rx.try_recv() {
            app.dirty = true;
            app.mark_idle();
            let preview: String = output
                .lines()
                .take(TOOL_PREVIEW_CAP)
                .collect::<Vec<_>>()
                .join("\n");
            let is_error = output.starts_with("error:") || output.contains("\nexit:");
            app.finish_tool(
                "bash",
                if preview.is_empty() {
                    "(no output)".into()
                } else {
                    preview
                },
                is_error,
            );
            // Ensure the finished card has the command as summary if we only
            // pushed a generic finish — re-open last bash card summary.
            if let Some(ChatItem::ToolCall {
                name,
                summary,
                done: true,
                ..
            }) = app.items.last_mut()
            {
                if name == "bash" && summary.is_empty() {
                    *summary = cmd.clone();
                    let i = app.items.len() - 1;
                    app.invalidate_item(i);
                }
            }
            if record {
                if let Ok(mut a) = agent.try_lock() {
                    a.messages.push(Message::user(format!(
                        "User ran a local command: `{cmd}`\nOutput:\n{output}"
                    )));
                }
            }
            app.notice(if record {
                format!("$ {cmd}  (recorded in context)")
            } else {
                format!("$ {cmd}  (not recorded)")
            });
        }

        let maybe_event = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            futures::StreamExt::next(&mut events),
        )
        .await;

        match maybe_event {
            Ok(Some(Ok(Event::Key(key)))) => {
                app.dirty = true;
                if handle_key(
                    &mut app,
                    key,
                    &prompt_tx,
                    &shell_tx,
                    &agent_for_cmds,
                    &controls_for_cmds,
                ) || app.should_quit
                {
                    break;
                }
            }
            Ok(Some(Ok(Event::Mouse(mouse)))) => {
                handle_mouse(&mut app, mouse);
            }
            Ok(Some(Ok(Event::Resize(_, _)))) => {
                app.dirty = true;
            }
            _ => {
                if app.running {
                    app.dirty = true;
                }
            }
        }

        if app.should_quit {
            break;
        }

        if !app.dirty {
            continue;
        }
        app.dirty = false;
        if std::env::var("PIRS_TUI_DEBUG").is_ok() {
            let dump = format!(
                "items={} scroll={} live={} running={} input={:?}\n",
                app.items.len(),
                app.scroll,
                app.live.is_some(),
                app.running,
                app.input
            );
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/tui_debug.log")
                .and_then(|mut f| std::io::Write::write_all(&mut f, dump.as_bytes()));
        }

        draw_dedup_cursor(&mut terminal, &mut app, &mut last_cursor)?;
        let frame_bytes = std::mem::take(terminal.backend_mut().writer_mut());
        tui_writer.push(frame_bytes);
    }

    // Make sure the writer thread has flushed the last frame it was given
    // before the restore escape sequences below write to the same real
    // stdout — otherwise they could race and interleave.
    tui_writer.shutdown();

    // Explicit restore before Drop (Drop is best-effort).
    restore_terminal()?;
    drop(_guard);

    // Session stats after the alternate screen is gone (qwen-code-style exit summary).
    {
        app.clock.agent_end();
        let report = {
            let a = agent.lock().await;
            a.usage_report()
        };
        session_stats::print_session_stats(
            &app.clock,
            &report,
            &app.model,
            app.plan_model.as_deref(),
            app.strategy.as_deref(),
        );
    }

    if let Some(h) = &opts.host {
        for err in h.drain_hook_errors() {
            eprintln!("[extension error] {err}");
        }
    }
    Ok(())
}

// welcome is ChatItem::Welcome (see render_welcome)

fn format_tokens(input: u64, output: u64, hit_pct: f64) -> String {
    format!(
        "in {} · out {} · {:.0}% cached",
        compact_num(input),
        compact_num(output),
        hit_pct
    )
}

fn compact_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ── Input handling ──────────────────────────────────────────────────────────

/// Returns true if the app should quit.
fn handle_key(
    app: &mut App,
    key: KeyEvent,
    prompt_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    shell_tx: &tokio::sync::mpsc::UnboundedSender<(String, String, bool)>,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
) -> bool {
    // Single-key approval answers when a gate is waiting.
    if app.pending_approval.lock().unwrap().is_some() {
        let grace_ok = approval_grace_elapsed(app.approval_opened_at);
        match (key.code, key.modifiers) {
            (KeyCode::Char('y') | KeyCode::Char('Y'), KeyModifiers::NONE)
            | (KeyCode::Char('n') | KeyCode::Char('N'), KeyModifiers::NONE)
            | (KeyCode::Char('a') | KeyCode::Char('A'), KeyModifiers::NONE) => {
                if !grace_ok {
                    return false;
                }
                let ch = match key.code {
                    KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
                    _ => "n".into(),
                };
                *app.pending_approval.lock().unwrap() = None;
                app.approval_opened_at = None;
                let _ = app.approval_answer.send(ch);
                app.set_status(String::new());
                return false;
            }
            (KeyCode::Enter, _) => {
                // Enter must not auto-confirm during grace (vibe pattern).
                if !grace_ok {
                    return false;
                }
            }
            (KeyCode::Esc, _) => {
                *app.pending_approval.lock().unwrap() = None;
                app.approval_opened_at = None;
                let _ = app.approval_answer.send("n".into());
                app.set_status(String::new());
                return false;
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => return true,
            _ => {}
        }
    }

    if app.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                app.show_help = false;
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            _ => {}
        }
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => return true,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.running {
                app.cancel.lock().unwrap().cancel();
                app.notice("cancel requested");
            } else {
                return true;
            }
        }
        (KeyCode::Esc, _) => {
            if app.running {
                app.cancel.lock().unwrap().cancel();
                app.notice("cancel requested");
            } else if !app.input.is_empty() {
                app.input.clear();
                app.cursor = 0;
            }
        }
        (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
            app.clear_chat();
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.thinking_expanded = !app.thinking_expanded;
            // Invalidate assistant rows so thinking re-renders.
            for i in 0..app.items.len() {
                if matches!(app.items[i], ChatItem::Assistant { .. }) {
                    app.invalidate_item(i);
                }
            }
            app.dirty = true;
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
            delete_word_before_cursor(app);
        }
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            app.cursor = 0;
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            app.cursor = app.input.len();
        }
        (KeyCode::Tab, KeyModifiers::NONE) if app.input.is_empty() => {
            app.toggle_last_tool_expand();
        }
        (KeyCode::Tab, KeyModifiers::NONE) if slash_completing(&app.input) => {
            apply_slash_completion(app);
        }
        (KeyCode::Char('?'), KeyModifiers::NONE) if app.input.is_empty() => {
            app.show_help = true;
        }
        (KeyCode::Char('1'), KeyModifiers::NONE) if app.input.is_empty() => {
            app.apply_starter(0);
        }
        (KeyCode::Char('2'), KeyModifiers::NONE) if app.input.is_empty() => {
            app.apply_starter(1);
        }
        (KeyCode::Char('3'), KeyModifiers::NONE) if app.input.is_empty() => {
            app.apply_starter(2);
        }
        (KeyCode::Char('g'), KeyModifiers::NONE) if app.input.is_empty() => {
            // Scroll to top of chat (gg-style single g).
            app.scroll = max_scroll(app);
            app.dirty = true;
        }
        (KeyCode::Char('G'), KeyModifiers::SHIFT) if app.input.is_empty() => {
            app.scroll = 0;
            app.dirty = true;
        }
        // Newline: alt/shift+enter, or ctrl-j (terminals vary).
        (KeyCode::Enter, KeyModifiers::ALT)
        | (KeyCode::Enter, KeyModifiers::SHIFT)
        | (KeyCode::Char('j'), KeyModifiers::CONTROL)
        | (KeyCode::Char('\n'), _) => {
            insert_at_cursor(app, '\n');
        }
        (KeyCode::Enter, _) => {
            // If slash popup is open and prefix is incomplete, complete first.
            if slash_completing(&app.input) {
                let matches = slash_filter(app.input.trim());
                if matches.len() == 1
                    || (matches.len() > 1
                        && matches
                            .get(app.slash_sel)
                            .map(|c| c.name != app.input.trim())
                            .unwrap_or(false))
                {
                    // Complete when unique, or when selection differs from typed prefix.
                    if matches.len() == 1
                        || matches
                            .get(app.slash_sel)
                            .is_some_and(|c| !app.input.trim().eq_ignore_ascii_case(c.name))
                    {
                        apply_slash_completion(app);
                        // Only auto-submit bare commands that take no args.
                        let cmd = app.input.trim().to_string();
                        if matches!(
                            cmd.as_str(),
                            "/help"
                                | "/tour"
                                | "/stats"
                                | "/usage"
                                | "/clear"
                                | "/quit"
                                | "/doctor"
                                | "/undo"
                                | "/compact"
                                | "/plan"
                                | "/act"
                        ) {
                            submit_input(app, prompt_tx, shell_tx, agent, controls);
                        }
                        return false;
                    }
                }
            }
            submit_input(app, prompt_tx, shell_tx, agent, controls);
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.input.clear();
            app.cursor = 0;
            app.history_idx = None;
            app.slash_sel = 0;
        }
        (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
            insert_at_cursor(app, c);
            if slash_completing(&app.input) {
                app.slash_sel = 0;
            }
        }
        (KeyCode::Backspace, _) => {
            delete_before_cursor(app);
        }
        (KeyCode::Delete, _) => {
            delete_after_cursor(app);
        }
        (KeyCode::Left, _) => {
            move_cursor_left(app);
        }
        (KeyCode::Right, _) => {
            move_cursor_right(app);
        }
        (KeyCode::Home, _) => {
            app.cursor = 0;
        }
        (KeyCode::End, _) => {
            app.cursor = app.input.len();
        }
        (KeyCode::Up, _) if slash_completing(&app.input) => {
            let n = slash_filter(app.input.trim()).len();
            if n > 0 {
                app.slash_sel = app.slash_sel.saturating_add(n - 1) % n;
            }
        }
        (KeyCode::Down, _) if slash_completing(&app.input) => {
            let n = slash_filter(app.input.trim()).len();
            if n > 0 {
                app.slash_sel = (app.slash_sel + 1) % n;
            }
        }
        (KeyCode::Up, _) => history_up(app),
        (KeyCode::Down, _) => history_down(app),
        (KeyCode::PageUp, _) => {
            let max = max_scroll(app);
            app.scroll = (app.scroll.saturating_add(5)).min(max);
        }
        (KeyCode::PageDown, _) => {
            app.scroll = app.scroll.saturating_sub(5);
        }
        _ => {}
    }
    false
}

fn apply_slash_completion(app: &mut App) {
    let matches = slash_filter(app.input.trim());
    if matches.is_empty() {
        return;
    }
    let idx = app.slash_sel.min(matches.len() - 1);
    let name = matches[idx].name;
    app.input = format!("{name} ");
    app.cursor = app.input.len();
    app.slash_sel = 0;
    app.history_idx = None;
    app.dirty = true;
}

fn delete_word_before_cursor(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    let chars: Vec<(usize, char)> = app.input[..app.cursor].char_indices().collect();
    if chars.is_empty() {
        return;
    }
    let mut idx = chars.len();
    while idx > 0 && chars[idx - 1].1.is_whitespace() {
        idx -= 1;
    }
    while idx > 0 && !chars[idx - 1].1.is_whitespace() {
        idx -= 1;
    }
    let i = if idx == 0 { 0 } else { chars[idx].0 };
    app.input.replace_range(i..app.cursor, "");
    app.cursor = i;
    app.history_idx = None;
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            let max = max_scroll(app);
            app.scroll = (app.scroll.saturating_add(3)).min(max);
            app.dirty = true;
        }
        MouseEventKind::ScrollDown => {
            app.scroll = app.scroll.saturating_sub(3);
            app.dirty = true;
        }
        _ => {}
    }
}

fn max_scroll(app: &App) -> u16 {
    // Real wrapped-row total from the last frame; draw re-clamps every frame.
    app.total_rows
        .saturating_sub(app.viewport_height as usize)
        .min(u16::MAX as usize) as u16
}

fn insert_at_cursor(app: &mut App, c: char) {
    app.input.insert(app.cursor, c);
    app.cursor += c.len_utf8();
    app.history_idx = None;
}

fn delete_before_cursor(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    let prev = app.input[..app.cursor]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
    app.input.drain(prev..app.cursor);
    app.cursor = prev;
    app.history_idx = None;
}

fn delete_after_cursor(app: &mut App) {
    if app.cursor >= app.input.len() {
        return;
    }
    let next = app.input[app.cursor..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| app.cursor + i)
        .unwrap_or(app.input.len());
    app.input.drain(app.cursor..next);
    app.history_idx = None;
}

fn move_cursor_left(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    app.cursor = app.input[..app.cursor]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
}

fn move_cursor_right(app: &mut App) {
    if app.cursor >= app.input.len() {
        return;
    }
    app.cursor = app.input[app.cursor..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| app.cursor + i)
        .unwrap_or(app.input.len());
}

fn history_up(app: &mut App) {
    if app.history.is_empty() {
        return;
    }
    match app.history_idx {
        None => {
            app.history_draft = app.input.clone();
            let idx = app.history.len() - 1;
            app.history_idx = Some(idx);
            app.input = app.history[idx].clone();
            app.cursor = app.input.len();
        }
        Some(0) => {}
        Some(i) => {
            let idx = i - 1;
            app.history_idx = Some(idx);
            app.input = app.history[idx].clone();
            app.cursor = app.input.len();
        }
    }
}

fn history_down(app: &mut App) {
    let Some(i) = app.history_idx else {
        return;
    };
    if i + 1 >= app.history.len() {
        app.history_idx = None;
        app.input = std::mem::take(&mut app.history_draft);
        app.cursor = app.input.len();
    } else {
        let idx = i + 1;
        app.history_idx = Some(idx);
        app.input = app.history[idx].clone();
        app.cursor = app.input.len();
    }
}

fn submit_input(
    app: &mut App,
    prompt_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    shell_tx: &tokio::sync::mpsc::UnboundedSender<(String, String, bool)>,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
) {
    let text = app.input.trim().to_string();
    if text.is_empty() {
        return;
    }
    app.input.clear();
    app.cursor = 0;
    app.history_idx = None;
    app.history_draft.clear();

    // Approval path (typed full answer).
    if app.pending_approval.lock().unwrap().is_some() {
        if !approval_grace_elapsed(app.approval_opened_at) {
            // Restore input so the user doesn't lose their typed answer.
            app.input = text;
            app.cursor = app.input.len();
            return;
        }
        *app.pending_approval.lock().unwrap() = None;
        app.approval_opened_at = None;
        let _ = app.approval_answer.send(text);
        return;
    }

    if text == "/quit" || text == "/exit" {
        app.should_quit = true;
        return;
    }
    if text == "/help" || text == "?" {
        app.show_help = true;
        return;
    }
    if text == "/clear" {
        app.clear_chat();
        return;
    }

    // Slash commands: model / plan-model / strategy (do not send to the agent).
    if text.starts_with('/') {
        handle_slash_command(app, agent, controls, &text);
        return;
    }

    // Local shell: `!cmd` records output in agent context; `!!cmd` does not.
    if text.starts_with('!') {
        if app.running {
            app.notice("busy — wait for the current run, then try !cmd again");
            return;
        }
        let (record, cmd) = if let Some(rest) = text.strip_prefix("!!") {
            (false, rest.trim())
        } else {
            (true, text[1..].trim())
        };
        if cmd.is_empty() {
            app.notice("usage: !cmd  (record)  or  !!cmd  (no record)");
            return;
        }
        if app.history.last().map(|h| h.as_str()) != Some(text.as_str()) {
            app.history.push(text.clone());
        }
        app.push(ChatItem::User(format!("$ {cmd}")));
        app.start_tool("bash".into(), cmd.to_string());
        app.mark_running(format!("shell · {cmd}"));
        let cwd = app.cwd.clone();
        let cmd_owned = cmd.to_string();
        let shell_tx = shell_tx.clone();
        std::thread::spawn(move || {
            let output = run_shell_command(&cwd, &cmd_owned);
            let _ = shell_tx.send((cmd_owned, output, record));
        });
        return;
    }

    if app.history.last().map(|h| h.as_str()) != Some(text.as_str()) {
        app.history.push(text.clone());
    }

    app.push(ChatItem::User(text.clone()));
    app.scroll = 0; // jump to the bottom to follow the new turn
    if app.running {
        app.steer_queue.lock().unwrap().push(text);
        app.last_activity = "steering".into();
        app.set_status("steering…");
    } else {
        app.mark_running(if app.strategy.is_some() {
            format!("strategy · {}", app.strategy.as_deref().unwrap_or("?"))
        } else {
            "thinking".into()
        });
        app.clock.mark_user_turn();
        app.clock.agent_start();
        // Snapshot conversation before this turn for /undo.
        if let Ok(a) = agent.try_lock() {
            pirs_tools::rewind_snapshot(
                &text.chars().take(80).collect::<String>(),
                &a.messages,
            );
        }
        let _ = prompt_tx.send(text);
    }
}

/// Run a local shell command (same spirit as REPL `!` / `!!`).
fn run_shell_command(cwd: &std::path::Path, cmd: &str) -> String {
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .output();
    match result {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            if !out.status.success() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&format!("exit: {}", out.status));
            }
            if s.is_empty() {
                "(no output)".into()
            } else {
                // Cap huge dumps so the chat stays usable.
                const MAX: usize = 16_000;
                if s.len() > MAX {
                    let tail: String = s.chars().skip(s.chars().count().saturating_sub(MAX)).collect();
                    format!("…(truncated)\n{tail}")
                } else {
                    s
                }
            }
        }
        Err(e) => format!("error: {e}"),
    }
}

fn handle_slash_command(
    app: &mut App,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
    text: &str,
) {
    let (cmd, arg) = match text.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (text, ""),
    };
    match cmd {
        "/help" | "/?" => {
            app.show_help = true;
            app.notice(
                "slash: /tour /model /plan-model /strategy /stats /undo /doctor /audit \
                 /profile /image /compact /plan /act /clear /quit  ·  type / + Tab",
            );
        }
        "/tour" | "/start" | "/onboard" => {
            app.push_tour_welcome();
            app.notice("tour restored — press 1–3 for starters, or type a goal");
        }
        "/model" => {
            if arg.is_empty() {
                let aliases = if app.model_aliases.is_empty() {
                    String::new()
                } else {
                    format!("\naliases: {}", app.model_aliases.join(", "))
                };
                app.notice(format!("model: {}{aliases}", app.model));
            } else {
                // Try_lock: agent worker may hold the lock while a turn runs.
                match agent.try_lock() {
                    Ok(mut a) => {
                        a.model = arg.to_string();
                        app.model = arg.to_string();
                        app.notice(format!("model → {arg}"));
                    }
                    Err(_) => {
                        app.notice("busy — wait for the current run to finish, then /model");
                    }
                }
            }
        }
        "/plan-model" => {
            if arg.is_empty() {
                app.notice(format!(
                    "plan-model: {}",
                    app.plan_model.as_deref().unwrap_or("(none — phases use --model)")
                ));
            } else if arg == "none" || arg == "off" || arg == "clear" {
                app.plan_model = None;
                controls.lock().unwrap().plan_model = None;
                app.notice("plan-model cleared");
            } else {
                app.plan_model = Some(arg.to_string());
                controls.lock().unwrap().plan_model = Some(arg.to_string());
                app.notice(format!("plan-model → {arg}"));
            }
        }
        "/strategy" => {
            if arg.is_empty() {
                app.notice(format!(
                    "strategy: {}\n  set: /strategy plan-exec | plan-critic-exec | monolithic\n  clear: /strategy none",
                    app.strategy.as_deref().unwrap_or("(none — plain agent loop)")
                ));
            } else if arg == "none" || arg == "off" || arg == "clear" {
                app.strategy = None;
                controls.lock().unwrap().strategy = None;
                app.notice("strategy cleared (plain agent loop)");
            } else {
                // Validate strategy resolves (builtin or file).
                match pirs_rhai::discover::resolve_strategy(arg, &std::env::current_dir().unwrap_or_default()) {
                    Ok(s) => {
                        app.strategy = Some(s.name.clone());
                        controls.lock().unwrap().strategy = Some(s.name.clone());
                        app.notice(format!(
                            "strategy → {} ({} step(s)); next message runs the strategy",
                            s.name,
                            s.steps.len()
                        ));
                    }
                    Err(e) => {
                        app.notice(format!("unknown strategy {arg:?}: {e}"));
                    }
                }
            }
        }
        "/usage" | "/stats" => {
            match agent.try_lock() {
                Ok(a) => {
                    let r = a.usage_report();
                    let text = session_stats::format_session_stats(
                        &app.clock,
                        &r,
                        &app.model,
                        app.plan_model.as_deref(),
                        app.strategy.as_deref(),
                    );
                    app.notice(text);
                }
                Err(_) => app.notice("busy — try /stats after the run finishes"),
            }
        }
        "/undo" => match agent.try_lock() {
            Ok(mut a) => match pirs_tools::host_undo(&mut a.messages) {
                Ok(msg) => {
                    app.notice(msg);
                    app.push(ChatItem::System("conversation rewound".into()));
                }
                Err(e) => app.notice(format!("undo: {e}")),
            },
            Err(_) => app.notice("busy — wait for the run, then /undo"),
        },
        "/doctor" => {
            let report = pirs_tools::doctor_report(&app.cwd).join("\n");
            app.push(ChatItem::System(report));
            app.notice("doctor report (see chat)");
        }
        "/audit" => {
            let n: usize = arg.parse().unwrap_or(30).clamp(1, 200);
            let path = pirs_agent::default_audit_path();
            let text = if !path.is_file() {
                format!("no audit log yet at {}", path.display())
            } else {
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<&str> = body.lines().collect();
                let start = lines.len().saturating_sub(n);
                format!(
                    "audit {} (last {} of {}):\n{}",
                    path.display(),
                    lines.len() - start,
                    lines.len(),
                    lines[start..].join("\n")
                )
            };
            app.push(ChatItem::System(text));
            app.notice("audit tail (see chat)");
        }
        "/profile" => {
            if arg.is_empty() {
                app.notice(format!(
                    "agent-profile: {}",
                    std::env::var("PIRS_AGENT_PROFILE").unwrap_or_else(|_| "default".into())
                ));
            } else if pirs_tools::SafetyProfile::parse(arg).is_some() {
                std::env::set_var("PIRS_AGENT_PROFILE", arg);
                app.notice(format!("agent-profile → {arg}"));
            } else {
                app.notice("usage: /profile default|plan|accept-edits|auto-approve");
            }
        }
        "/image" => {
            if arg.is_empty() {
                app.notice("usage: /image <path.png|jpg|webp>");
            } else {
                match agent.try_lock() {
                    Ok(mut a) => match attach_image_to_agent(&mut a, &app.cwd, arg) {
                        Ok(msg) => {
                            app.notice(msg);
                            app.push(ChatItem::System(format!("image attached: {arg}")));
                        }
                        Err(e) => app.notice(format!("image: {e}")),
                    },
                    Err(_) => app.notice("busy — wait, then /image"),
                }
            }
        }
        "/compact" => {
            if agent.try_lock().is_err() {
                app.notice("busy — try /compact after the run");
            } else {
                let agent = Arc::clone(agent);
                tokio::spawn(async move {
                    let mut a = agent.lock().await;
                    let _ = a.compact_now().await;
                });
                app.notice("compact started (messages may shrink after next turn)");
            }
        }
        "/voice" => {
            app.notice(
                "voice: use pirs-claw with speech backends (STT/TTS), or set \
                 PIRS_STT_BACKEND / PIRS_TTS_BACKEND. TUI live mic is planned — \
                 paste transcript or use Telegram voice notes via claw.",
            );
        }
        "/plan" => {
            pirs_tools::set_live_permission_mode(pirs_tools::PermissionMode::ReadOnly);
            std::env::set_var("PIRS_AGENT_PROFILE", "plan");
            app.notice("mode → plan (read-only tools; switch with /act)");
        }
        "/act" => {
            pirs_tools::set_live_permission_mode(pirs_tools::PermissionMode::DangerFullAccess);
            app.notice("mode → act (full tools; plan with /plan)");
        }
        "/permission" => {
            if arg.is_empty() {
                app.notice(format!(
                    "permission: {}",
                    pirs_tools::live_permission_mode().name()
                ));
            } else if let Some(m) = pirs_tools::PermissionMode::parse(arg) {
                pirs_tools::set_live_permission_mode(m);
                app.notice(format!("permission → {}", m.name()));
            } else {
                app.notice("usage: /permission read-only|workspace-write|danger-full-access");
            }
        }
        "/checkpoint" => {
            let action = if arg.is_empty() { "list" } else { arg };
            match action {
                "list" => {
                    let list = pirs_tools::list_checkpoints(&app.cwd);
                    if list.is_empty() {
                        app.notice("no checkpoints");
                    } else {
                        let mut s = String::from("checkpoints:\n");
                        for m in list {
                            s.push_str(&format!("{} {}\n", m.id, m.label));
                        }
                        app.push(ChatItem::System(s));
                    }
                }
                "create" => match agent.try_lock() {
                    Ok(a) => match pirs_tools::create_checkpoint(
                        &app.cwd,
                        "tui",
                        a.messages.len(),
                    ) {
                        Ok(m) => app.notice(format!("checkpoint {}", m.id)),
                        Err(e) => app.notice(format!("checkpoint: {e}")),
                    },
                    Err(_) => app.notice("busy — try later"),
                },
                s if s.starts_with("restore") => {
                    let id = s.split_whitespace().nth(1);
                    match pirs_tools::restore_checkpoint(&app.cwd, id) {
                        Ok(msg) => app.notice(msg),
                        Err(e) => app.notice(format!("restore: {e}")),
                    }
                }
                _ => app.notice("usage: /checkpoint list|create|restore [id]"),
            }
        }
        other => {
            app.notice(format!(
                "unknown command {other} — /help for slash list"
            ));
        }
    }
}

fn attach_image_to_agent(
    agent: &mut Agent,
    cwd: &std::path::Path,
    path: &str,
) -> anyhow::Result<String> {
    use base64::Engine as _;
    let p = std::path::Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    if !abs.is_file() {
        anyhow::bail!("not found: {}", abs.display());
    }
    let bytes = std::fs::read(&abs)?;
    if bytes.len() > 12 * 1024 * 1024 {
        anyhow::bail!("image too large");
    }
    let mime = match abs
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        other => anyhow::bail!("unsupported .{other}"),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    agent.messages.push(Message::User(pirs_ai::UserMessage {
        content: pirs_ai::UserContent::Blocks(vec![
            pirs_ai::ContentBlock::Text {
                text: format!("[image attached: {}]", abs.display()),
                text_signature: None,
            },
            pirs_ai::ContentBlock::Image {
                data: b64,
                mime_type: mime.into(),
            },
        ]),
        timestamp: pirs_ai::now_millis(),
    }));
    Ok(format!("attached {} ({} bytes)", abs.display(), bytes.len()))
}

// ── Drawing ─────────────────────────────────────────────────────────────────

/// Replicates `Terminal::try_draw`'s sequence (autoresize, render, flush,
/// cursor, swap, backend flush) using ratatui's public lower-level pieces,
/// but only re-emits the cursor escape (`Hide`, or `Show`+`MoveTo`) when
/// `app.desired_cursor` actually differs from the previous frame's.
///
/// `Terminal::draw`/`try_draw` themselves have no such gate — every call
/// unconditionally calls `hide_cursor()` or `show_cursor()`+
/// `set_cursor_position()` regardless of whether the position/visibility
/// changed (confirmed in both ratatui 0.29's `Terminal::try_draw` and
/// ratatui-core 0.1.2's `apply_buffer_with_cursor`, the 0.30 successor —
/// unfixed upstream, not something bumping the dependency would resolve).
/// On most terminals a `Show`/`MoveTo` write resets the cursor's blink
/// phase, so during active token streaming — which redraws the frame many
/// times a second while the input-box cursor itself isn't moving — the
/// stock behavior makes the cursor look like it never blinks at all.
/// Reusable, `App`/`draw_ui`-independent version of `draw_dedup_cursor`'s
/// mechanism: `render` draws the frame and returns the cursor position it
/// wants (or `None` to hide it); the escape is only re-emitted when that
/// differs from `last_cursor`. Kept generic and decoupled from `App` so the
/// dedup behavior itself is unit-testable without constructing a full `App`.
fn draw_with_cursor_dedup<B, F>(
    terminal: &mut Terminal<B>,
    last_cursor: &mut Option<(u16, u16)>,
    render: F,
) -> anyhow::Result<()>
where
    B: ratatui::backend::Backend,
    F: FnOnce(&mut ratatui::Frame) -> Option<(u16, u16)>,
{
    terminal.autoresize()?;
    let desired = {
        let mut frame = terminal.get_frame();
        render(&mut frame)
    };
    terminal.flush()?;

    if desired != *last_cursor {
        match desired {
            None => terminal.hide_cursor()?,
            Some(pos) => {
                terminal.show_cursor()?;
                terminal.set_cursor_position(pos)?;
            }
        }
        *last_cursor = desired;
    }

    terminal.swap_buffers();
    terminal.backend_mut().flush()?;
    Ok(())
}

/// Renders one TUI frame with the cursor-blink-preserving dedup wrapper —
/// see `draw_with_cursor_dedup` and `App::desired_cursor` for why this
/// exists instead of a plain `terminal.draw(|frame| draw_ui(frame, app))`.
fn draw_dedup_cursor<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    last_cursor: &mut Option<(u16, u16)>,
) -> anyhow::Result<()> {
    draw_with_cursor_dedup(terminal, last_cursor, |frame| {
        app.desired_cursor = None;
        draw_ui(frame, app);
        app.desired_cursor
    })
}

fn draw_ui(frame: &mut ratatui::Frame, app: &mut App) {
    let theme = Theme::default_dark();

    // Leave the last row unused: writing the bottom-right corner cell scrolls
    // the terminal and corrupts every subsequent frame.
    let full = frame.area();
    let area = Rect {
        height: full.height.saturating_sub(1),
        ..full
    };

    let input_lines = app.input.lines().count().clamp(1, 6) as u16;
    let input_h = input_lines + 2; // borders
    let pending = app.pending_approval.lock().unwrap().is_some();
    // Approval modal needs a taller status strip for the overlay cue.
    let status_h: u16 = if pending { 1 } else { 1 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // header
            Constraint::Min(3),          // chat
            Constraint::Length(status_h), // turn-status
            Constraint::Length(input_h), // input
        ])
        .split(area);

    draw_header(frame, chunks[0], app, &theme);
    draw_chat(frame, chunks[1], app, &theme);
    draw_status(frame, chunks[2], app, &theme);
    draw_input(frame, chunks[3], app, &theme);

    if slash_completing(&app.input) && !pending {
        draw_slash_popup(frame, chunks[3], app, &theme);
    }
    if pending {
        draw_approval_overlay(frame, area, app, &theme);
    }
    if app.show_help {
        draw_help_overlay(frame, area, &theme);
    }
}

fn draw_slash_popup(frame: &mut ratatui::Frame, input_area: Rect, app: &App, theme: &Theme) {
    let matches = slash_filter(app.input.trim());
    if matches.is_empty() {
        return;
    }
    let show_n = matches.len().min(8) as u16;
    let h = show_n + 2; // borders
    let w = input_area.width.min(56).max(28);
    let y = input_area.y.saturating_sub(h);
    let rect = Rect {
        x: input_area.x,
        y,
        width: w,
        height: h.min(input_area.y + input_area.height), // stay on screen
    };
    if rect.height < 3 {
        return;
    }
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(Span::styled(" commands · tab complete · ↑↓ ", theme.dim));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let sel = app.slash_sel.min(matches.len().saturating_sub(1));
    // Window so selection stays visible.
    let max_rows = inner.height as usize;
    let start = if sel >= max_rows {
        sel + 1 - max_rows
    } else {
        0
    };
    let mut lines = Vec::new();
    for (i, cmd) in matches.iter().enumerate().skip(start).take(max_rows) {
        let selected = i == sel;
        let style = if selected {
            theme.brand.add_modifier(Modifier::REVERSED)
        } else {
            theme.assistant_text
        };
        let desc_style = if selected {
            theme.brand.add_modifier(Modifier::REVERSED)
        } else {
            theme.dim
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:<14}", cmd.name), style),
            Span::styled(cmd.desc.to_string(), desc_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_header(frame: &mut ratatui::Frame, area: Rect, app: &App, theme: &Theme) {
    // Thin identity chrome; token usage lives on the turn-status row (qwen footer pattern).
    let mode_style = composer_mode_style(theme, &app.approval_mode, false, false);
    let mut left = vec![
        Span::styled(" pirs ", theme.brand),
        Span::styled("│ ", theme.dim),
        Span::styled(app.model.clone(), theme.header_bg),
    ];
    if let Some(p) = &app.plan_model {
        left.push(Span::styled(" plan:", theme.dim));
        left.push(Span::styled(p.clone(), theme.plan));
    }
    if let Some(s) = &app.strategy {
        left.push(Span::styled(" strat:", theme.dim));
        left.push(Span::styled(s.clone(), theme.accent));
    }
    left.push(Span::styled("  ", theme.dim));
    left.push(Span::styled(
        format!("● {}", app.approval_mode),
        mode_style,
    ));
    left.push(Span::styled("  ", theme.dim));
    left.push(Span::styled(
        format!("~/{}", app.cwd_label),
        theme.header_bg,
    ));
    let clipped = clip_spans(left, area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(clipped)), area);
}

/// Slack (in wrapped rows, not items) kept exactly measured on each side of
/// the viewport. Generous enough to absorb the usual case — a handful of
/// never-yet-measured items pushed since the last frame — without
/// repeatedly re-measuring/evicting right at the viewport's edge on small
/// scrolls.
const VIRTUALIZE_MARGIN_ROWS: usize = 200;

fn draw_chat(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border)
        .title(Span::styled(" chat ", theme.dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.max(1) as usize;
    let vh = inner.height as usize;
    app.viewport_height = inner.height;

    let prev_total = app.total_rows;
    let width_stable = app.last_draw_width == width;

    // A resize invalidates exact measurements (wrapping depends on width),
    // but keeps each item's previous row_count as a placeholder estimate —
    // deferring the full re-wrap instead of doing it immediately, the same
    // way pushing a new item no longer forces one either (see ItemCache).
    if app.cache_width != width {
        for c in &mut app.item_caches {
            c.rows = None;
        }
        app.cache_width = width;
    }
    // App::push can't measure (no theme/width there), so new items arrive
    // as bare placeholders; nothing to do here beyond the invariant that
    // item_caches.len() == items.len(), which push already maintains.
    debug_assert_eq!(app.item_caches.len(), app.items.len());

    // The live streaming preview changes every frame (blinking cursor / new
    // tokens), so it is wrapped fresh each time — only the tail, cheap.
    let mut live_rows: Vec<Line<'static>> = Vec::new();
    if let Some((thinking, text)) = &app.live {
        let mut logical: Vec<Line<'static>> = vec![Line::from(vec![
            Span::styled("│ ", theme.assistant_label),
            Span::styled("assistant", theme.assistant_label),
            Span::styled("  streaming", theme.dim),
        ])];
        if !thinking.trim().is_empty() {
            // While streaming, show thinking expanded lightly (last lines).
            logical.extend(render_thinking(thinking, theme, true));
        }
        if !text.trim().is_empty() {
            logical.extend(render_markdown(text, theme, width.saturating_sub(2)));
        }
        let cursor = if (app.tick / 4).is_multiple_of(2) {
            "▌"
        } else {
            " "
        };
        logical.push(Line::from(Span::styled(
            format!("  {cursor}"),
            theme.accent,
        )));
        live_rows = flatten_rows(&logical, width);
    }

    // Pass 1: using the current (possibly stale) row_count estimates, work
    // out roughly where the viewport sits, exactly measure any item near
    // it, and evict the exact rows of anything far from it so a long
    // session with large tool outputs doesn't hold every item's wrapped
    // text in memory at once. VIRTUALIZE_MARGIN_ROWS of slack on each side
    // means a small scroll doesn't repeatedly re-measure/evict at the
    // boundary, and comfortably covers the usual case of a handful of
    // never-yet-measured items (new pushes since the last frame).
    {
        let total_est: usize = app.item_caches.iter().map(|c| c.row_count).sum();
        let max_skip_est = total_est.saturating_sub(vh);
        let scroll_est = (app.scroll as usize).min(max_skip_est);
        let start_est = max_skip_est.saturating_sub(scroll_est);
        let end_est = start_est + vh;

        let mut offset = 0usize;
        for i in 0..app.items.len() {
            let item_start = offset;
            let item_end = item_start + app.item_caches[i].row_count;
            offset = item_end;
            let near = item_end + VIRTUALIZE_MARGIN_ROWS > start_est
                && item_start < end_est + VIRTUALIZE_MARGIN_ROWS;
            if near {
                if app.item_caches[i].rows.is_none() {
                    let logical = app.items[i].render(theme, width, app.thinking_expanded);
                    let rows = flatten_rows(&logical, width);
                    app.item_caches[i].row_count = rows.len();
                    app.item_caches[i].rows = Some(rows);
                }
            } else if app.item_caches[i].rows.is_some() {
                app.item_caches[i].rows = None;
            }
        }
    }

    // Pass 2: now that pass 1 corrected any stale/placeholder row_count
    // near the viewport, compute the real totals and clamp scroll against
    // them — same semantics as before, just measured incrementally.
    let total_items_rows: usize = app.item_caches.iter().map(|c| c.row_count).sum();
    let total = total_items_rows + live_rows.len();
    app.total_rows = total;
    let max_skip = total.saturating_sub(vh);

    // Keep the reading position stable when scrolled up: as new rows arrive,
    // grow the from-bottom offset by the same amount so the view doesn't drift.
    // When pinned (scroll == 0) we simply follow the bottom.
    if app.scroll > 0 && width_stable {
        let grew = total.saturating_sub(prev_total);
        if grew > 0 {
            app.scroll = (app.scroll as usize + grew).min(u16::MAX as usize) as u16;
        }
    }
    app.scroll = app.scroll.min(max_skip.min(u16::MAX as usize) as u16);

    let start = max_skip.saturating_sub(app.scroll as usize);
    let end = start + vh;

    // Pass 3: paint. Items overlapping [start, end) should already be
    // exactly measured by pass 1's margin; the `rows.is_none()` fallback
    // here is a correctness backstop (never skip painting an item just
    // because an estimate was off), not the expected common path.
    let mut visible: Vec<Line<'static>> = Vec::with_capacity(vh.min(total.max(1)));
    let mut offset = 0usize;
    for i in 0..app.items.len() {
        let item_start = offset;
        let row_count = app.item_caches[i].row_count;
        let item_end = item_start + row_count;
        offset = item_end;
        if item_end <= start || item_start >= end {
            continue;
        }
        if app.item_caches[i].rows.is_none() {
            let logical = app.items[i].render(theme, width, app.thinking_expanded);
            let rows = flatten_rows(&logical, width);
            app.item_caches[i].row_count = rows.len();
            app.item_caches[i].rows = Some(rows);
        }
        let rows = app.item_caches[i].rows.as_ref().unwrap();
        let local_start = start.saturating_sub(item_start);
        let local_end = (end.saturating_sub(item_start)).min(rows.len());
        if local_start < local_end {
            visible.extend(rows[local_start..local_end].iter().cloned());
        }
    }
    let live_start = start.saturating_sub(total_items_rows);
    let live_end = (end.saturating_sub(total_items_rows)).min(live_rows.len());
    if live_start < live_end {
        visible.extend(live_rows[live_start..live_end].iter().cloned());
    }
    visible.truncate(vh);

    // Rows are pre-wrapped to `width`; render without ratatui's wrap so the
    // painted layout matches the row model used for scrolling.
    frame.render_widget(Paragraph::new(visible), inner);

    // Scrollbar thumb on the right edge when content overflows.
    if max_skip > 0 && area.width > 2 && vh > 0 {
        let ratio = 1.0 - (app.scroll as f64 / max_skip as f64);
        let thumb_y = inner.y + ((vh.saturating_sub(1) as f64) * ratio) as u16;
        if thumb_y < inner.y + inner.height {
            frame.render_widget(
                Paragraph::new(Span::styled("▐", theme.accent)),
                Rect {
                    x: area.x + area.width.saturating_sub(1),
                    y: thumb_y,
                    width: 1,
                    height: 1,
                },
            );
        }
    }

    app.last_draw_width = width;
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    // ~7.5–10 fps spinner (tick advances every dirty frame while running).
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    app.tick = app.tick.wrapping_add(1);

    let mut left: Vec<Span<'static>> = Vec::new();
    let mut right: Vec<Span<'static>> = Vec::new();

    let approval_q = app.pending_approval.lock().unwrap().clone();
    if let Some(q) = approval_q {
        left.push(Span::styled(" ◆ ", theme.approval));
        left.push(Span::styled("waiting for approval", theme.approval));
        left.push(Span::styled(
            format!(" · {}", truncate_chars(&q, 48)),
            theme.dim,
        ));
        right.push(Span::styled(" y / a / n · esc ", theme.dim));
    } else if app.running {
        let spin = FRAMES[(app.tick / 2 % 10) as usize];
        left.push(Span::styled(format!(" {spin} "), theme.accent));
        let activity = if !app.last_activity.is_empty() {
            app.last_activity.as_str()
        } else if !app.status_msg.is_empty() {
            app.status_msg.as_str()
        } else {
            "working"
        };
        left.push(Span::styled(activity.to_string(), theme.status));
        if let Some(start) = app.turn_started_at {
            left.push(Span::styled(
                format!(" · {}", format_elapsed(start.elapsed().as_secs())),
                theme.dim,
            ));
        }
        left.push(Span::styled("  ·  type to steer", theme.dim));
        right.push(Span::styled(" esc cancel ", theme.dim));
    } else {
        left.push(Span::styled(" ○ ", theme.dim));
        left.push(Span::styled("ready", theme.status));
        if !app.status_msg.is_empty() {
            left.push(Span::styled(format!("  ·  {}", app.status_msg), theme.dim));
        }
        left.push(Span::styled("  ·  ? help  ·  tab expand tool", theme.dim));
    }

    if app.scroll > 0 {
        right.insert(
            0,
            Span::styled(format!(" ↑{} ", app.scroll), theme.accent),
        );
    }
    if !app.usage_summary.is_empty() && !app.running {
        right.insert(
            0,
            Span::styled(format!(" {} ", app.usage_summary), theme.dim),
        );
    }

    // Paint left, then right-align remainder.
    let right_w: usize = right
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let left_budget = (area.width as usize).saturating_sub(right_w);
    let left_clipped = clip_spans(left, left_budget);
    let left_w: usize = left_clipped
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let pad = (area.width as usize).saturating_sub(left_w + right_w);
    let mut line = left_clipped;
    if pad > 0 {
        line.push(Span::raw(" ".repeat(pad)));
    }
    line.extend(right);
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}

fn draw_input(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    let pending = app.pending_approval.lock().unwrap().is_some();
    let border_style = composer_mode_style(theme, &app.approval_mode, app.running, pending);
    let title = if pending {
        " approval · [y]es  [a]lways session  [n]o  esc "
    } else if app.running {
        " ❯ steer · enter queue · esc cancel "
    } else {
        match app.approval_mode.to_ascii_lowercase().as_str() {
            m if m.contains("yolo") || m == "auto" => " ❯ yolo · enter send · ? help ",
            m if m.contains("plan") => " ❯ plan · enter send · /act to write ",
            m if m.contains("ask") => " ❯ ask · enter send · approvals on ",
            _ => " ❯ message · enter send · alt+enter newline ",
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, theme.dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (display, style) = if app.input.is_empty() && !pending {
        (
            "message · enter send · ? help".to_string(),
            theme.placeholder,
        )
    } else {
        (
            app.input.clone(),
            if pending { theme.approval } else { theme.input },
        )
    };
    let para = Paragraph::new(display.as_str())
        .style(style)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);

    // Cursor position accounting for multi-line wrap.
    let cursor_text = if app.input.is_empty() && !pending {
        // Keep cursor at start over placeholder.
        ""
    } else {
        &app.input[..app.cursor.min(app.input.len())]
    };
    let (cx, cy) = cursor_pos(cursor_text, inner.width.max(1) as usize);
    let cursor_x = (inner.x + cx as u16).min(inner.x + inner.width.saturating_sub(1));
    let cursor_y = (inner.y + cy as u16).min(inner.y + inner.height.saturating_sub(1));
    app.desired_cursor = Some((cursor_x, cursor_y));
}

fn draw_approval_overlay(frame: &mut ratatui::Frame, area: Rect, app: &App, theme: &Theme) {
    let question = app
        .pending_approval
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    let w = area.width.clamp(48, 72);
    let h = area.height.clamp(10, 14);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.approval)
        .title(Span::styled(" approval required ", theme.approval));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let grace = if approval_grace_elapsed(app.approval_opened_at) {
        ""
    } else {
        "  (wait…)"
    };
    let lines = vec![
        Line::from(Span::styled(
            "  The agent wants to run a sensitive tool.",
            theme.assistant_text,
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", truncate_chars(&question, (w as usize).saturating_sub(4))),
            theme.command,
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [y] ", theme.success),
            Span::styled("Yes once", theme.assistant_text),
        ]),
        Line::from(vec![
            Span::styled("  [a] ", theme.warning),
            Span::styled("Always this session", theme.assistant_text),
        ]),
        Line::from(vec![
            Span::styled("  [n] ", theme.error),
            Span::styled("No / deny", theme.assistant_text),
        ]),
        Line::from(Span::styled(
            format!("  esc = deny{grace}"),
            theme.dim,
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn cursor_pos(text_before_cursor: &str, width: usize) -> (usize, usize) {
    if width == 0 {
        return (0, 0);
    }
    let mut x = 0usize;
    let mut y = 0usize;
    for ch in text_before_cursor.chars() {
        if ch == '\n' {
            x = 0;
            y += 1;
            continue;
        }
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if x + w > width {
            y += 1;
            x = w;
        } else {
            x += w;
        }
    }
    (x, y)
}

fn draw_help_overlay(frame: &mut ratatui::Frame, area: Rect, theme: &Theme) {
    let w = area.width.clamp(48, 72);
    let h = area.height.clamp(18, 28);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(Span::styled(" help ", theme.brand));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled("Keys", theme.heading)),
        Line::from(Span::styled(
            "  enter / alt+enter   send / newline (ctrl-j)",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ↑↓ pgup/pgdn wheel  history / scroll chat",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  g / G (empty)       scroll top / bottom",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  tab (empty)         expand/collapse last tool",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-o              expand/collapse thoughts",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-w / ctrl-u     delete word / clear input",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-a / ctrl-e     line start / end",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  esc / ctrl-c        cancel run · ctrl-d quit",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-l              clear screen  ·  ? help",
            theme.assistant_text,
        )),
        Line::from(""),
        Line::from(Span::styled("Commands", theme.heading)),
        Line::from(Span::styled(
            "  /tour /model /plan-model /strategy /stats",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /plan /act /permission /profile /checkpoint",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /undo /compact /doctor /audit /image",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /clear /quit  ·  !cmd  !!cmd (shell)",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  type / then Tab · 1–3 starters when empty",
            theme.dim,
        )),
        Line::from(""),
        Line::from(Span::styled("  esc / q / ? to close", theme.dim)),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── Scroll helpers ──────────────────────────────────────────────────────────

fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Flatten logical lines into physical rows, each no wider than `width`, so the
/// row model is authoritative: the chat is rendered from these rows with no
/// further wrapping, so scroll math and paint can never disagree.
fn flatten_rows(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in lines {
        out.extend(wrap_line_to_rows(l, width));
    }
    out
}

/// Word-wrap one logical line into physical rows ≤ `width`, preserving span
/// styles. Overlong tokens (e.g. long paths) are hard-split; a space at a wrap
/// seam is dropped so continuation rows start on a word.
fn wrap_line_to_rows(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let w = width.max(1);
    if line_width(line) <= w {
        return vec![line.clone()];
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for span in &line.spans {
        let style = span.style;
        for token in split_keep_spaces(span.content.as_ref()) {
            let tok_w = unicode_width::UnicodeWidthStr::width(token);
            if cur_w + tok_w <= w {
                cur.push(Span::styled(token.to_string(), style));
                cur_w += tok_w;
            } else if tok_w > w {
                // Token wider than a full row: hard-split by display columns.
                for ch in token.chars() {
                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    if cur_w + ch_w > w && cur_w > 0 {
                        rows.push(Line::from(std::mem::take(&mut cur)));
                        cur_w = 0;
                    }
                    cur.push(Span::styled(ch.to_string(), style));
                    cur_w += ch_w;
                }
            } else {
                rows.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
                if token.chars().all(|c| c == ' ') {
                    continue; // drop the space that fell on the seam
                }
                cur.push(Span::styled(token.to_string(), style));
                cur_w = tok_w;
            }
        }
    }
    rows.push(Line::from(cur));
    rows
}

/// Split a string into alternating runs of spaces and non-spaces, keeping both.
fn split_keep_spaces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut cur_space: Option<bool> = None;
    for (i, ch) in s.char_indices() {
        let is_sp = ch == ' ';
        match cur_space {
            None => cur_space = Some(is_sp),
            Some(prev) if prev != is_sp => {
                out.push(&s[start..i]);
                start = i;
                cur_space = Some(is_sp);
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Clip a run of spans to `width` display columns, preserving each span's style
/// (unlike collapsing everything to one style after joining to a string).
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let sw = unicode_width::UnicodeWidthStr::width(span.content.as_ref());
        if used + sw <= width {
            used += sw;
            out.push(span);
        } else {
            let remaining = width - used;
            let mut buf = String::new();
            let mut bw = 0usize;
            for ch in span.content.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                if bw + cw > remaining {
                    break;
                }
                buf.push(ch);
                bw += cw;
            }
            if !buf.is_empty() {
                out.push(Span::styled(buf, span.style));
            }
            break;
        }
    }
    out
}

// ── Agent events ────────────────────────────────────────────────────────────

fn apply_agent_event(app: &mut App, event: AgentEvent) {
    match event {
        AgentEvent::MessageStart { message } => {
            if message.is_assistant() {
                app.live = Some((String::new(), String::new()));
                app.last_activity = "streaming".into();
                app.dirty = true;
            }
        }
        AgentEvent::MessageUpdate { message } => {
            if app.live.is_none() {
                return;
            }
            let thinking = extract_thinking(&message);
            let text = message.text();
            if !thinking.is_empty() && text.trim().is_empty() {
                app.last_activity = "thinking".into();
            } else {
                app.last_activity = "streaming".into();
            }
            if app.last_live_refresh.elapsed() < std::time::Duration::from_millis(80) {
                // Always keep latest content even if we skip a paint.
                app.live = Some((thinking, text));
                return;
            }
            app.last_live_refresh = std::time::Instant::now();
            app.live = Some((thinking, text));
            app.dirty = true;
        }
        AgentEvent::MessageEnd { message } => {
            if let Message::Assistant(a) = *message {
                app.live = None;
                let thinking = extract_thinking(&a);
                let text = a.text();
                let error = if a.stop_reason == pirs_ai::StopReason::Error {
                    Some(a.error_message.unwrap_or_else(|| "unknown error".into()))
                } else {
                    None
                };
                if !thinking.trim().is_empty() || !text.trim().is_empty() || error.is_some() {
                    app.push(ChatItem::Assistant {
                        thinking,
                        text,
                        error,
                    });
                }
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => {
            let summary = crate::summarize_args(&tool_name, &args);
            app.start_tool(tool_name, summary);
        }
        AgentEvent::ToolExecutionEnd { result, tool_name, .. } => {
            app.clock.mark_tool(result.is_error);
            // Prefer details.uiText (full) over model-capped content for display.
            let text = result.display_text();
            let preview: String = text
                .lines()
                .take(TOOL_PREVIEW_CAP)
                .collect::<Vec<_>>()
                .join("\n");
            let body = if preview.is_empty() && result.is_error {
                "(error)".into()
            } else {
                preview
            };
            if !body.is_empty() || result.is_error {
                app.finish_tool(&tool_name, body, result.is_error);
            } else {
                // Success with empty body — still mark done.
                app.finish_tool(&tool_name, String::new(), false);
            }
        }
        AgentEvent::CompactionStart { .. } => {
            app.last_activity = "compacting".into();
            app.notice("compacting context…");
        }
        AgentEvent::CompactionEnd { aborted, .. } => {
            if aborted {
                app.notice("compaction skipped");
            } else {
                app.notice("compaction done");
            }
        }
        AgentEvent::TurnStart => {
            app.last_activity = "thinking".into();
            app.set_status("thinking");
        }
        AgentEvent::TurnEnd { .. } => {
            app.last_activity = "running".into();
            app.set_status("running");
        }
        AgentEvent::AgentStart => {
            if app.turn_started_at.is_none() {
                app.turn_started_at = Some(std::time::Instant::now());
            }
            app.last_activity = "running".into();
            app.set_status("running");
        }
        AgentEvent::AgentEnd { .. } => {
            app.set_status(String::new());
        }
        _ => {}
    }
}

fn extract_thinking(a: &pirs_ai::AssistantMessage) -> String {
    a.content
        .iter()
        .filter_map(|b| match b {
            pirs_ai::ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                Some(thinking.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Approval bridge ─────────────────────────────────────────────────────────

fn approval_bridge(
    opts: &mut TuiOptions,
) -> (
    Arc<Mutex<Option<String>>>,
    Arc<std::sync::mpsc::Sender<String>>,
) {
    let pending: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(gate) = &opts.approval_gate {
        let pending = Arc::clone(&pending);
        let rx = Arc::new(std::sync::Mutex::new(rx));
        gate.set_prompter(move |question| {
            *pending.lock().unwrap() = Some(question.to_string());
            rx.lock()
                .unwrap()
                .recv()
                .unwrap_or_else(|_| "n".to_string())
        });
    }
    (pending, Arc::new(tx))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps `ratatui::backend::TestBackend`, counting cursor-escape calls so
    /// `draw_with_cursor_dedup`'s dedup behavior can be asserted mechanically
    /// (there's no terminal to visually watch blink in a test).
    struct CountingBackend {
        inner: ratatui::backend::TestBackend,
        hide_calls: u32,
        show_calls: u32,
        move_calls: u32,
    }

    impl CountingBackend {
        fn new(w: u16, h: u16) -> Self {
            CountingBackend {
                inner: ratatui::backend::TestBackend::new(w, h),
                hide_calls: 0,
                show_calls: 0,
                move_calls: 0,
            }
        }
    }

    impl ratatui::backend::Backend for CountingBackend {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.draw(content)
        }
        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.hide_calls += 1;
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.show_calls += 1;
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            self.move_calls += 1;
            self.inner.set_cursor_position(position)
        }
        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.clear()
        }
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.size()
        }
        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size()
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    fn cursor_dedup_skips_reemission_when_position_is_unchanged() {
        let mut terminal = Terminal::new(CountingBackend::new(20, 5)).unwrap();
        let mut last_cursor = None;

        // Frame 1: cursor appears at (2, 1) — first time, must emit.
        draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
            frame.render_widget(Paragraph::new("hi"), frame.area());
            Some((2, 1))
        })
        .unwrap();
        assert_eq!(terminal.backend().show_calls, 1);
        assert_eq!(terminal.backend().move_calls, 1);

        // Frame 2: content changes (simulating streamed tokens) but the
        // cursor position is identical — must NOT re-emit.
        draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
            frame.render_widget(Paragraph::new("hi there, more text"), frame.area());
            Some((2, 1))
        })
        .unwrap();
        assert_eq!(
            terminal.backend().show_calls,
            1,
            "unchanged cursor position must not re-trigger Show"
        );
        assert_eq!(
            terminal.backend().move_calls,
            1,
            "unchanged cursor position must not re-trigger MoveTo"
        );

        // Frame 3: cursor actually moves — must emit again.
        draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
            frame.render_widget(Paragraph::new("hi there"), frame.area());
            Some((5, 1))
        })
        .unwrap();
        assert_eq!(terminal.backend().show_calls, 2);
        assert_eq!(terminal.backend().move_calls, 2);

        // Frame 4: cursor hidden entirely — must emit hide once, then...
        draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
            frame.render_widget(Paragraph::new("hi there"), frame.area());
            None
        })
        .unwrap();
        assert_eq!(terminal.backend().hide_calls, 1);

        // Frame 5: still hidden — must not re-emit hide either.
        draw_with_cursor_dedup(&mut terminal, &mut last_cursor, |frame| {
            frame.render_widget(Paragraph::new("hi there"), frame.area());
            None
        })
        .unwrap();
        assert_eq!(
            terminal.backend().hide_calls,
            1,
            "unchanged (hidden) cursor state must not re-trigger Hide"
        );
    }

    #[test]
    fn wrap_words_respects_width() {
        let lines = wrap_words("hello beautiful world", 10);
        assert!(lines
            .iter()
            .all(|l| unicode_width::UnicodeWidthStr::width(l.as_str()) <= 10));
        assert!(lines.len() >= 2);
    }

    #[test]
    fn wrap_words_empty() {
        assert_eq!(wrap_words("", 10), vec![""]);
    }

    #[test]
    fn truncate_chars_shortens() {
        let s = truncate_chars("abcdefghijklmnopqrstuvwxyz", 10);
        assert!(s.chars().count() <= 10);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn inline_spans_code_and_bold() {
        let theme = Theme::default_dark();
        let spans = inline_spans("use `foo` and **bar** ok", &theme);
        let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(joined, "use foo and bar ok");
        assert!(spans.len() >= 5);
    }

    #[test]
    fn render_markdown_code_fence() {
        let theme = Theme::default_dark();
        let md = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = render_markdown(md, &theme, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("fn main()"));
        assert!(text.contains("╭─ rust") || text.contains("rust"));
    }

    #[test]
    fn compact_num_formats() {
        assert_eq!(compact_num(42), "42");
        assert_eq!(compact_num(1500), "1.5k");
        assert_eq!(compact_num(2_500_000), "2.5M");
    }

    #[test]
    fn cursor_pos_multiline() {
        assert_eq!(cursor_pos("hi", 80), (2, 0));
        assert_eq!(cursor_pos("hi\nthere", 80), (5, 1));
    }

    #[test]
    fn chat_item_user_renders_label() {
        let theme = Theme::default_dark();
        let lines = ChatItem::User("hello".into()).render(&theme, 80, false);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("|");
        assert!(flat.contains("you"));
        assert!(flat.contains("hello"));
    }

    #[test]
    fn clear_chat_keeps_items_and_caches_in_sync() {
        let mut app = test_app();
        for i in 0..5 {
            app.push(ChatItem::Notice(format!("n{i}")));
        }
        assert_eq!(app.items.len(), 5);
        assert_eq!(app.item_caches.len(), 5);
        app.clear_chat();
        assert_eq!(app.items.len(), 1, "notice after clear");
        assert_eq!(
            app.item_caches.len(),
            app.items.len(),
            "caches must stay length-aligned with items"
        );
        assert!(matches!(app.items[0], ChatItem::Notice(_)));
    }

    #[test]
    fn tool_quiet_read_collapses_on_success() {
        let theme = Theme::default_dark();
        let item = ChatItem::ToolCall {
            name: "read".into(),
            summary: "src/main.rs".into(),
            preview: "fn main() {}\n// more\n// lines".into(),
            is_error: false,
            done: true,
            expanded: false,
        };
        let lines = item.render(&theme, 80, false);
        assert_eq!(lines.len(), 1, "collapsed read is header-only: {lines:?}");
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat.contains("Read"), "{flat}");
        assert!(flat.contains("src/main.rs"), "{flat}");
    }

    #[test]
    fn tool_bash_expanded_shows_l_border_body() {
        let theme = Theme::default_dark();
        let item = ChatItem::ToolCall {
            name: "bash".into(),
            summary: "cargo test".into(),
            preview: "ok\npassed".into(),
            is_error: false,
            done: true,
            expanded: true,
        };
        let lines = item.render(&theme, 80, false);
        assert!(lines.len() > 1, "bash body should expand: {lines:?}");
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat.contains("Ran") || flat.contains("bash"), "{flat}");
        assert!(flat.contains("⎢") || flat.contains("⎣"), "{flat}");
    }

    #[test]
    fn thinking_collapsed_by_default() {
        let theme = Theme::default_dark();
        let item = ChatItem::Assistant {
            thinking: "line1\nline2\nline3".into(),
            text: "hi".into(),
            error: None,
        };
        let collapsed = item.render(&theme, 80, false);
        let flat: String = collapsed
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat.contains("▶ thought"), "{flat}");
        assert!(!flat.contains("line1"), "body hidden when collapsed: {flat}");

        let expanded = item.render(&theme, 80, true);
        let flat_e: String = expanded
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat_e.contains("line1"), "{flat_e}");
    }

    #[test]
    fn tool_verb_and_default_expand_policy() {
        assert_eq!(tool_verb("bash", false), "Running");
        assert_eq!(tool_verb("bash", true), "Ran");
        assert_eq!(tool_verb("read", true), "Read");
        assert!(!tool_default_expanded("read", false));
        assert!(tool_default_expanded("bash", false));
        assert!(tool_default_expanded("read", true)); // errors expand
    }

    #[test]
    fn approval_grace_blocks_until_elapsed() {
        assert!(approval_grace_elapsed(None));
        let recent = Some(std::time::Instant::now());
        assert!(!approval_grace_elapsed(recent));
        let old = Some(std::time::Instant::now() - std::time::Duration::from_millis(500));
        assert!(approval_grace_elapsed(old));
    }

    #[test]
    fn composer_mode_styles_differ_for_yolo_and_plan() {
        let theme = Theme::default_dark();
        let idle = composer_mode_style(&theme, "ask", false, false);
        let yolo = composer_mode_style(&theme, "yolo", false, false);
        let plan = composer_mode_style(&theme, "plan", false, false);
        let pending = composer_mode_style(&theme, "ask", false, true);
        assert_ne!(yolo.fg, idle.fg);
        assert_ne!(plan.fg, yolo.fg);
        assert_eq!(pending.fg, theme.approval.fg);
    }

    #[test]
    fn finish_tool_updates_open_card_in_place() {
        let mut app = test_app();
        app.start_tool("read".into(), "a.rs".into());
        assert_eq!(app.items.len(), 1);
        app.finish_tool("read", "contents".into(), false);
        assert_eq!(app.items.len(), 1, "must not push a second card");
        match &app.items[0] {
            ChatItem::ToolCall {
                done,
                expanded,
                preview,
                ..
            } => {
                assert!(*done);
                assert!(!*expanded, "quiet read stays collapsed");
                assert_eq!(preview, "contents");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn quiet_tools_collapse_into_verb_group() {
        let mut app = test_app();
        for path in ["a.rs", "b.rs", "c.rs"] {
            app.start_tool("read".into(), path.into());
            app.finish_tool("read", format!("// {path}"), false);
        }
        assert_eq!(app.items.len(), 1, "three reads → one group: {:?}", app.items);
        match &app.items[0] {
            ChatItem::ToolGroup {
                name,
                members,
                expanded,
            } => {
                assert_eq!(name, "read");
                assert_eq!(members.len(), 3);
                assert!(!*expanded);
            }
            other => panic!("expected ToolGroup, got {other:?}"),
        }
        let theme = Theme::default_dark();
        let lines = app.items[0].render(&theme, 80, false);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat.contains("Read 3 files"), "{flat}");
    }

    #[test]
    fn edit_preview_uses_diff_colors() {
        let theme = Theme::default_dark();
        let item = ChatItem::ToolCall {
            name: "edit".into(),
            summary: "x.rs".into(),
            preview: " context\n-old\n+new\n".into(),
            is_error: false,
            done: true,
            expanded: true,
        };
        let lines = item.render(&theme, 80, false);
        // Find styled + / - lines
        let mut saw_plus = false;
        let mut saw_minus = false;
        for line in &lines {
            for span in &line.spans {
                if span.content.contains("+new") {
                    saw_plus = true;
                    assert_eq!(span.style.fg, theme.success.fg);
                }
                if span.content.contains("-old") {
                    saw_minus = true;
                    assert_eq!(span.style.fg, theme.tool_err.fg);
                }
            }
        }
        assert!(saw_plus && saw_minus, "diff lines should be present: {lines:?}");
    }

    #[test]
    fn slash_filter_matches_prefix() {
        let m = slash_filter("/mo");
        assert!(m.iter().any(|c| c.name == "/model"), "{m:?}");
        assert!(!m.iter().any(|c| c.name == "/quit"));
        let all = slash_filter("/");
        assert!(all.len() >= 10);
    }

    #[test]
    fn slash_completion_applies_selected() {
        let mut app = test_app();
        app.input = "/mo".into();
        app.cursor = 3;
        app.slash_sel = 0;
        apply_slash_completion(&mut app);
        assert!(app.input.starts_with("/model"), "{}", app.input);
        assert!(app.input.ends_with(' '));
    }

    #[test]
    fn starter_fills_input() {
        let mut app = test_app();
        app.apply_starter(0);
        assert!(app.input.contains("repository"));
        assert_eq!(app.cursor, app.input.len());
    }

    #[test]
    fn welcome_first_run_mentions_starters() {
        let theme = Theme::default_dark();
        let item = ChatItem::Welcome {
            model: "m".into(),
            plan_model: None,
            strategy: None,
            approval: "ask".into(),
            cwd: "proj".into(),
            first_run: true,
        };
        let lines = item.render(&theme, 80, false);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(flat.contains("Getting started"), "{flat}");
        assert!(flat.contains("Explain this repo"), "{flat}");
    }

    #[test]
    fn wrap_line_to_rows_wraps_long_line() {
        let theme = Theme::default_dark();
        let long = "word ".repeat(40); // ~200 cols with spaces
        let line = Line::from(Span::styled(long, theme.assistant_text));
        let rows = wrap_line_to_rows(&line, 20);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|r| line_width(r) <= 20));
    }

    #[test]
    fn wrap_line_to_rows_fast_path_when_fits() {
        let theme = Theme::default_dark();
        let line = Line::from(Span::styled("short".to_string(), theme.assistant_text));
        assert_eq!(wrap_line_to_rows(&line, 20).len(), 1);
    }

    #[test]
    fn wrap_line_hard_splits_overlong_token() {
        let theme = Theme::default_dark();
        let line = Line::from(Span::styled("x".repeat(50), theme.assistant_text));
        let rows = wrap_line_to_rows(&line, 10);
        assert!(rows.len() >= 5);
        assert!(rows.iter().all(|r| line_width(r) <= 10));
    }

    #[test]
    fn wrap_line_preserves_span_styles() {
        let theme = Theme::default_dark();
        let line = Line::from(vec![
            Span::styled("aaaa bbbb ".to_string(), theme.accent),
            Span::styled("cccc dddd eeee".to_string(), theme.error),
        ]);
        let rows = wrap_line_to_rows(&line, 8);
        // Every span in every row keeps one of the two original styles.
        for r in &rows {
            for s in &r.spans {
                assert!(s.style == theme.accent || s.style == theme.error);
            }
        }
    }

    #[test]
    fn split_keep_spaces_alternates() {
        assert_eq!(split_keep_spaces("ab  cd"), vec!["ab", "  ", "cd"]);
        assert_eq!(split_keep_spaces("  x"), vec!["  ", "x"]);
        assert_eq!(split_keep_spaces("x"), vec!["x"]);
    }

    #[test]
    fn clip_spans_bounds_width_and_keeps_style() {
        let theme = Theme::default_dark();
        let spans = vec![
            Span::styled("hello ".to_string(), theme.accent),
            Span::styled("world!!!".to_string(), theme.error),
        ];
        let clipped = clip_spans(spans, 8);
        let w: usize = clipped
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert!(w <= 8);
        assert_eq!(clipped[0].style, theme.accent);
    }

    #[test]
    fn flatten_rows_all_within_width() {
        let theme = Theme::default_dark();
        let lines = vec![
            Line::from(Span::styled("short".to_string(), theme.assistant_text)),
            Line::from(Span::styled("a ".repeat(30), theme.assistant_text)),
        ];
        let rows = flatten_rows(&lines, 16);
        assert!(rows.len() >= 3);
        assert!(rows.iter().all(|r| line_width(r) <= 16));
    }

    #[test]
    fn tool_status_glyphs() {
        assert_eq!(tool_status_glyph(false, false, 0).0, "○");
        assert_eq!(tool_status_glyph(true, false, 0).0, "✓");
        assert_eq!(tool_status_glyph(true, true, 0).0, "✗");
    }

    #[test]
    fn latest_slot_overwrites_unconsumed_value_rather_than_queuing() {
        // The core backpressure behavior: pushing B then C before anyone
        // takes A's successor must leave only C, not queue B behind A.
        let slot: LatestSlot<u32> = LatestSlot::new();
        slot.push(1);
        assert_eq!(slot.take_blocking(), Some(1));
        slot.push(2);
        slot.push(3);
        assert_eq!(
            slot.take_blocking(),
            Some(3),
            "second push should replace, not queue behind, the first"
        );
    }

    #[test]
    fn latest_slot_wakes_a_blocked_consumer() {
        // A consumer waiting on an empty slot must be woken by a later push,
        // not just see a value that was already there when it started.
        let slot = Arc::new(LatestSlot::<u32>::new());
        let consumer_slot = Arc::clone(&slot);
        let handle = std::thread::spawn(move || consumer_slot.take_blocking());

        std::thread::sleep(std::time::Duration::from_millis(50));
        slot.push(42);

        assert_eq!(handle.join().unwrap(), Some(42));
    }

    #[test]
    fn latest_slot_close_releases_a_blocked_consumer_with_none() {
        let slot = Arc::new(LatestSlot::<u32>::new());
        let consumer_slot = Arc::clone(&slot);
        let handle = std::thread::spawn(move || consumer_slot.take_blocking());

        std::thread::sleep(std::time::Duration::from_millis(50));
        slot.close();

        assert_eq!(handle.join().unwrap(), None);
    }

    #[test]
    fn latest_slot_close_still_yields_a_value_pushed_before_it() {
        let slot: LatestSlot<u32> = LatestSlot::new();
        slot.push(7);
        slot.close();
        assert_eq!(
            slot.take_blocking(),
            Some(7),
            "a value pushed before close must still be delivered"
        );
        assert_eq!(
            slot.take_blocking(),
            None,
            "nothing left after that -> consumer should stop"
        );
    }

    #[test]
    fn tui_writer_push_and_shutdown_do_not_hang() {
        // Doesn't assert on stdout content (nothing to intercept without
        // redesigning TuiWriter around a generic writer), but proves the
        // full spawn -> push -> shutdown lifecycle actually terminates
        // rather than leaving the background thread parked forever.
        let writer = TuiWriter::spawn();
        writer.push(b"hello".to_vec());
        writer.push(b"world".to_vec());
        writer.shutdown();
    }

    /// A minimal but fully valid `App`, for tests that need to drive
    /// `draw_chat` directly rather than just its pure helper functions.
    fn test_app() -> App {
        App {
            items: Vec::new(),
            live: None,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            history_draft: String::new(),
            running: false,
            tick: 0,
            dirty: true,
            last_live_refresh: std::time::Instant::now(),
            steer_queue: Arc::new(Mutex::new(Vec::new())),
            scroll: 0,
            viewport_height: 10,
            model: "test-model".into(),
            plan_model: None,
            strategy: None,
            model_aliases: Vec::new(),
            approval_mode: "auto".into(),
            cwd: PathBuf::from("."),
            cwd_label: ".".into(),
            usage_summary: String::new(),
            pending_approval: Arc::new(Mutex::new(None)),
            approval_answer: Arc::new(std::sync::mpsc::channel().0),
            approval_opened_at: None,
            cancel: Arc::new(Mutex::new(tokio_util::sync::CancellationToken::new())),
            show_help: false,
            status_msg: String::new(),
            last_activity: String::new(),
            turn_started_at: None,
            thinking_expanded: false,
            slash_sel: 0,
            first_run_session: false,
            should_quit: false,
            item_caches: Vec::new(),
            cache_width: 0,
            total_rows: 0,
            last_draw_width: 0,
            desired_cursor: None,
            clock: SessionClock::new(),
        }
    }

    fn draw_chat_once(app: &mut App, width: u16, height: u16) -> ratatui::backend::TestBackend {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default_dark();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_chat(frame, area, app, &theme);
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn backend_text(backend: &ratatui::backend::TestBackend) -> String {
        backend
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }

    #[test]
    fn draw_chat_pinned_to_bottom_shows_newest_items_and_evicts_far_off_screen() {
        let mut app = test_app();
        for i in 0..2000 {
            app.push(ChatItem::Notice(format!("item-{i:04}")));
        }
        // Pinned to the bottom (scroll == 0): the newest items should be
        // visible, and the oldest ones — far from the viewport — should not
        // still be holding exact rows in memory after this draw.
        let backend = draw_chat_once(&mut app, 40, 5);
        let text = backend_text(&backend);
        assert!(text.contains("item-1999"), "{text}");
        assert!(
            !text.contains("item-0000"),
            "the oldest item shouldn't be in a 5-row viewport pinned to the bottom: {text}"
        );
        assert!(
            app.item_caches[0].rows.is_none(),
            "item far from the viewport should have its exact rows evicted"
        );
        assert!(
            app.item_caches[1999].rows.is_some(),
            "item actually painted must have exact rows cached"
        );
    }

    #[test]
    fn draw_chat_scrolled_to_top_measures_top_items_and_evicts_the_bottom() {
        let mut app = test_app();
        for i in 0..2000 {
            app.push(ChatItem::Notice(format!("item-{i:04}")));
        }
        // First draw pinned to the bottom, so the tail is measured/cached...
        draw_chat_once(&mut app, 40, 5);
        assert!(app.item_caches[1999].rows.is_some());

        // ...then scroll all the way to the top and redraw.
        app.scroll = u16::MAX;
        let backend = draw_chat_once(&mut app, 40, 5);
        let text = backend_text(&backend);
        assert!(text.contains("item-0000"), "{text}");
        assert!(
            app.item_caches[0].rows.is_some(),
            "now-visible top item must be measured"
        );
        assert!(
            app.item_caches[1999].rows.is_none(),
            "no-longer-visible bottom item should have been evicted: far from the new viewport"
        );
    }

    #[test]
    fn draw_chat_resize_remeasures_items_at_the_new_width() {
        let mut app = test_app();
        // Long enough to wrap differently at 60 cols vs 20.
        app.push(ChatItem::Notice("word ".repeat(30)));
        draw_chat_once(&mut app, 60, 20);
        let rows_at_60 = app.item_caches[0].row_count;
        assert!(app.item_caches[0].rows.is_some());

        draw_chat_once(&mut app, 20, 20);
        let rows_at_20 = app.item_caches[0].row_count;
        assert!(
            rows_at_20 > rows_at_60,
            "the same text should wrap into more rows at a narrower width: \
             {rows_at_20} rows at 20 cols vs {rows_at_60} at 60 cols"
        );
        assert!(
            app.item_caches[0].rows.is_some(),
            "the item is in view at both widths, so it should be re-measured, not left stale"
        );
    }

    #[test]
    fn draw_chat_new_items_are_placeholders_until_actually_drawn() {
        let mut app = test_app();
        app.push(ChatItem::Notice("only item".into()));
        assert!(
            app.item_caches[0].rows.is_none(),
            "App::push has no width/theme to measure with"
        );
        draw_chat_once(&mut app, 40, 10);
        assert!(
            app.item_caches[0].rows.is_some(),
            "the first draw after a push should measure it once it's in view"
        );
    }
}
