//! Interactive terminal UI for pirs (`--mode tui`).
//!
//! Layout (top → bottom):
//!   header  — brand · model · approval · cwd · usage
//!   chat    — structured messages, tools, system notes
//!   status  — spinner / hints / scroll position
//!   input   — multi-line composer with history

use std::sync::{Arc, Mutex};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
    MouseEventKind,
};
use crossterm::ExecutableCommand;
use pirs_agent::{Agent, AgentEvent};
use pirs_ai::Message;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;

use crate::approval::ApprovalMode;

// ── Theme ───────────────────────────────────────────────────────────────────

struct Theme {
    brand: Style,
    header_bg: Style,
    user_label: Style,
    user_text: Style,
    assistant_label: Style,
    assistant_text: Style,
    thinking: Style,
    tool_name: Style,
    tool_args: Style,
    tool_ok: Style,
    tool_err: Style,
    system: Style,
    error: Style,
    dim: Style,
    accent: Style,
    border: Style,
    input: Style,
    input_border: Style,
    approval: Style,
    status: Style,
    code: Style,
    code_block: Style,
    bold: Style,
    heading: Style,
}

impl Theme {
    fn default_dark() -> Self {
        Self {
            brand: Style::default()
                .fg(Color::Rgb(125, 211, 252))
                .add_modifier(Modifier::BOLD),
            header_bg: Style::default().fg(Color::Rgb(148, 163, 184)),
            user_label: Style::default()
                .fg(Color::Rgb(52, 211, 153))
                .add_modifier(Modifier::BOLD),
            user_text: Style::default().fg(Color::Rgb(209, 250, 229)),
            assistant_label: Style::default()
                .fg(Color::Rgb(167, 139, 250))
                .add_modifier(Modifier::BOLD),
            assistant_text: Style::default().fg(Color::Rgb(226, 232, 240)),
            thinking: Style::default()
                .fg(Color::Rgb(100, 116, 139))
                .add_modifier(Modifier::ITALIC),
            tool_name: Style::default()
                .fg(Color::Rgb(251, 191, 36))
                .add_modifier(Modifier::BOLD),
            tool_args: Style::default().fg(Color::Rgb(148, 163, 184)),
            tool_ok: Style::default().fg(Color::Rgb(100, 116, 139)),
            tool_err: Style::default().fg(Color::Rgb(248, 113, 113)),
            system: Style::default().fg(Color::Rgb(100, 116, 139)),
            error: Style::default()
                .fg(Color::Rgb(248, 113, 113))
                .add_modifier(Modifier::BOLD),
            dim: Style::default().fg(Color::Rgb(71, 85, 105)),
            accent: Style::default().fg(Color::Rgb(56, 189, 248)),
            border: Style::default().fg(Color::Rgb(51, 65, 85)),
            input: Style::default().fg(Color::Rgb(241, 245, 249)),
            input_border: Style::default().fg(Color::Rgb(56, 189, 248)),
            approval: Style::default()
                .fg(Color::Rgb(251, 113, 133))
                .add_modifier(Modifier::BOLD),
            status: Style::default().fg(Color::Rgb(148, 163, 184)),
            code: Style::default().fg(Color::Rgb(125, 211, 252)),
            code_block: Style::default().fg(Color::Rgb(186, 230, 253)),
            bold: Style::default()
                .fg(Color::Rgb(248, 250, 252))
                .add_modifier(Modifier::BOLD),
            heading: Style::default()
                .fg(Color::Rgb(165, 243, 252))
                .add_modifier(Modifier::BOLD),
        }
    }
}

// ── Structured chat ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum ChatItem {
    System(String),
    User(String),
    Assistant {
        thinking: String,
        text: String,
        error: Option<String>,
    },
    ToolStart {
        name: String,
        summary: String,
    },
    ToolEnd {
        preview: String,
        is_error: bool,
    },
    Notice(String),
}

impl ChatItem {
    fn render(&self, theme: &Theme, width: usize) -> Vec<Line<'static>> {
        match self {
            ChatItem::System(text) => text
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), theme.system)))
                .collect(),
            ChatItem::User(text) => {
                let mut out = vec![Line::from(vec![
                    Span::styled("● ", theme.user_label),
                    Span::styled("you", theme.user_label),
                ])];
                for l in text.lines() {
                    out.push(Line::from(Span::styled(format!("  {l}"), theme.user_text)));
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
                    Span::styled("✦ ", theme.assistant_label),
                    Span::styled("assistant", theme.assistant_label),
                ])];
                if !thinking.trim().is_empty() {
                    out.extend(render_thinking(thinking, theme));
                }
                if !text.trim().is_empty() {
                    out.extend(render_markdown(text, theme, width.saturating_sub(2)));
                }
                if let Some(err) = error {
                    out.push(Line::from(Span::styled(format!("  ⚠ {err}"), theme.error)));
                }
                out.push(Line::from(""));
                out
            }
            ChatItem::ToolStart { name, summary } => {
                let icon = tool_icon(name);
                let mut spans = vec![
                    Span::styled(format!("  {icon} "), theme.tool_name),
                    Span::styled(name.clone(), theme.tool_name),
                ];
                if !summary.is_empty() {
                    spans.push(Span::styled("  ", theme.dim));
                    spans.push(Span::styled(truncate_chars(summary, 100), theme.tool_args));
                }
                vec![Line::from(spans)]
            }
            ChatItem::ToolEnd { preview, is_error } => {
                if preview.is_empty() {
                    return Vec::new();
                }
                let style = if *is_error {
                    theme.tool_err
                } else {
                    theme.tool_ok
                };
                let marker = if *is_error { "✗" } else { "·" };
                let mut out = Vec::new();
                for (i, l) in preview.lines().take(8).enumerate() {
                    let prefix = if i == 0 {
                        format!("    {marker} ")
                    } else {
                        "      ".into()
                    };
                    out.push(Line::from(Span::styled(format!("{prefix}{l}"), style)));
                }
                let extra = preview.lines().count().saturating_sub(8);
                if extra > 0 {
                    out.push(Line::from(Span::styled(
                        format!("      … +{extra} lines"),
                        theme.dim,
                    )));
                }
                out
            }
            ChatItem::Notice(text) => vec![Line::from(Span::styled(
                format!("  · {text}"),
                theme.system,
            ))],
        }
    }
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "bash" => "▸",
        "read" => "◉",
        "write" | "edit" => "✎",
        "grep" | "find" => "⌕",
        "ls" => "☰",
        "delegate" | "run_subagent" => "⧉",
        _ => "○",
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
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

fn render_thinking(thinking: &str, theme: &Theme) -> Vec<Line<'static>> {
    const MAX: usize = 6;
    let lines: Vec<&str> = thinking.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    let skip = total.saturating_sub(MAX);
    let mut out = Vec::new();
    if skip > 0 {
        out.push(Line::from(Span::styled(
            format!("  ⋯ thinking ({total} lines)"),
            theme.thinking,
        )));
    }
    for l in lines.into_iter().skip(skip) {
        out.push(Line::from(Span::styled(
            format!("  💭 {l}"),
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
    approval_mode: String,
    cwd_label: String,
    usage_summary: String,
    pending_approval: Arc<Mutex<Option<String>>>,
    approval_answer: Arc<std::sync::mpsc::Sender<String>>,
    cancel: pirs_agent::agent::CancelSlot,
    show_help: bool,
    status_msg: String,
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
        approval_mode: approval_name,
        cwd_label,
        usage_summary: String::new(),
        pending_approval,
        approval_answer: approval_answer_rx,
        cancel,
        show_help: false,
        status_msg: String::new(),
        should_quit: false,
        item_caches: Vec::new(),
        cache_width: 0,
        total_rows: 0,
        last_draw_width: 0,
        desired_cursor: None,
    };

    app.push(ChatItem::System(welcome_banner(
        &app.model,
        &app.approval_mode,
        &app.cwd_label,
    )));

    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    let agent = Arc::new(tokio::sync::Mutex::new(opts.agent));
    {
        let agent = Arc::clone(&agent);
        let done_tx = done_tx.clone();
        tokio::spawn(async move {
            while let Some(text) = prompt_rx.recv().await {
                let mut a = agent.lock().await;
                let result = a.prompt(&text).await;
                drop(a);
                let _ = done_tx.send(result.is_ok());
            }
        });
    }

    let mut events = crossterm::event::EventStream::new();
    let mut last_cursor: Option<(u16, u16)> = None;
    loop {
        while let Ok(event) = event_rx.try_recv() {
            apply_agent_event(&mut app, event);
        }
        while let Ok(ok) = done_rx.try_recv() {
            app.running = false;
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

        let maybe_event = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            futures::StreamExt::next(&mut events),
        )
        .await;

        match maybe_event {
            Ok(Some(Ok(Event::Key(key)))) => {
                app.dirty = true;
                if handle_key(&mut app, key, &prompt_tx) || app.should_quit {
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

    if let Some(h) = &opts.host {
        for err in h.drain_hook_errors() {
            eprintln!("[extension error] {err}");
        }
    }
    Ok(())
}

fn welcome_banner(model: &str, approval: &str, cwd: &str) -> String {
    format!(
        "pirs  ·  {model}  ·  approval:{approval}  ·  {cwd}\n\
         enter send · shift+enter newline · ↑↓ history · wheel/pgup scroll · ? help · esc cancel · ctrl-d quit"
    )
}

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
) -> bool {
    // Single-key approval answers when a gate is waiting.
    if app.pending_approval.lock().unwrap().is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Char('y') | KeyCode::Char('Y'), KeyModifiers::NONE)
            | (KeyCode::Char('n') | KeyCode::Char('N'), KeyModifiers::NONE)
            | (KeyCode::Char('a') | KeyCode::Char('A'), KeyModifiers::NONE) => {
                let ch = match key.code {
                    KeyCode::Char(c) => c.to_ascii_lowercase().to_string(),
                    _ => "n".into(),
                };
                *app.pending_approval.lock().unwrap() = None;
                let _ = app.approval_answer.send(ch);
                app.set_status(String::new());
                return false;
            }
            (KeyCode::Esc, _) => {
                *app.pending_approval.lock().unwrap() = None;
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
            app.items.clear();
            app.live = None;
            app.scroll = 0;
            app.notice("screen cleared");
        }
        (KeyCode::Char('?'), KeyModifiers::NONE) if app.input.is_empty() => {
            app.show_help = true;
        }
        // Newline: alt/shift+enter, or ctrl-j (terminals vary).
        (KeyCode::Enter, KeyModifiers::ALT)
        | (KeyCode::Enter, KeyModifiers::SHIFT)
        | (KeyCode::Char('j'), KeyModifiers::CONTROL)
        | (KeyCode::Char('\n'), _) => {
            insert_at_cursor(app, '\n');
        }
        (KeyCode::Enter, _) => {
            submit_input(app, prompt_tx);
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            app.input.clear();
            app.cursor = 0;
            app.history_idx = None;
        }
        (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
            insert_at_cursor(app, c);
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

fn submit_input(app: &mut App, prompt_tx: &tokio::sync::mpsc::UnboundedSender<String>) {
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
        *app.pending_approval.lock().unwrap() = None;
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
        app.items.clear();
        app.live = None;
        app.scroll = 0;
        app.notice("screen cleared");
        return;
    }

    if app.history.last().map(|h| h.as_str()) != Some(text.as_str()) {
        app.history.push(text.clone());
    }

    app.push(ChatItem::User(text.clone()));
    app.scroll = 0; // jump to the bottom to follow the new turn
    if app.running {
        app.steer_queue.lock().unwrap().push(text);
        app.set_status("steering…");
    } else {
        app.running = true;
        app.set_status("running");
        let _ = prompt_tx.send(text);
    }
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // header
            Constraint::Min(3),          // chat
            Constraint::Length(1),       // status
            Constraint::Length(input_h), // input
        ])
        .split(area);

    draw_header(frame, chunks[0], app, &theme);
    draw_chat(frame, chunks[1], app, &theme);
    draw_status(frame, chunks[2], app, &theme);
    draw_input(frame, chunks[3], app, &theme);

    if app.show_help {
        draw_help_overlay(frame, area, &theme);
    }
}

fn draw_header(frame: &mut ratatui::Frame, area: Rect, app: &App, theme: &Theme) {
    let usage = if app.usage_summary.is_empty() {
        String::new()
    } else {
        format!(" {} ", app.usage_summary)
    };
    let usage_w = unicode_width::UnicodeWidthStr::width(usage.as_str()) as u16;
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(usage_w.max(1))])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" pirs ", theme.brand),
            Span::styled("│ ", theme.dim),
            Span::styled(app.model.clone(), theme.header_bg),
            Span::styled("  ", theme.dim),
            Span::styled(format!("● {}", app.approval_mode), theme.accent),
            Span::styled("  ", theme.dim),
            Span::styled(format!("~/{}", app.cwd_label), theme.header_bg),
        ])),
        parts[0],
    );
    if !usage.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(usage, theme.dim)).alignment(Alignment::Right),
            parts[1],
        );
    }
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
            Span::styled("✦ ", theme.assistant_label),
            Span::styled("assistant", theme.assistant_label),
            Span::styled("  streaming", theme.dim),
        ])];
        if !thinking.trim().is_empty() {
            logical.extend(render_thinking(thinking, theme));
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
                    let logical = app.items[i].render(theme, width);
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
            let logical = app.items[i].render(theme, width);
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
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    app.tick = app.tick.wrapping_add(1);

    let mut parts: Vec<Span<'static>> = Vec::new();

    let approval_q = app.pending_approval.lock().unwrap().clone();
    if let Some(q) = approval_q {
        parts.push(Span::styled(" ⚠ approval ", theme.approval));
        parts.push(Span::styled(truncate_chars(&q, 60), theme.approval));
        parts.push(Span::styled("  [y]es [n]o [a]ll  esc=deny", theme.dim));
    } else if app.running {
        let spin = FRAMES[(app.tick / 2 % 10) as usize];
        parts.push(Span::styled(format!(" {spin} "), theme.accent));
        let label = if app.status_msg.is_empty() {
            "working"
        } else {
            app.status_msg.as_str()
        };
        parts.push(Span::styled(label.to_string(), theme.status));
        parts.push(Span::styled("  ·  type to steer  ·  esc cancel", theme.dim));
    } else {
        parts.push(Span::styled(" ○ ", theme.dim));
        parts.push(Span::styled("ready", theme.status));
        if !app.status_msg.is_empty() {
            parts.push(Span::styled(format!("  ·  {}", app.status_msg), theme.dim));
        }
        parts.push(Span::styled("  ·  ? help", theme.dim));
    }

    if app.scroll > 0 {
        parts.push(Span::styled(format!("  ·  ↑{} ", app.scroll), theme.accent));
    }

    let clipped = clip_spans(parts, area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(clipped)), area);
}

fn draw_input(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    let pending = app.pending_approval.lock().unwrap().is_some();
    let (title, border_style) = if pending {
        (" approval · y / n / a ", theme.approval)
    } else if app.running {
        (" message · steers the running turn ", theme.input_border)
    } else {
        (
            " message · enter send · alt+enter newline ",
            theme.input_border,
        )
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, theme.dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Show a subtle prompt glyph on the first line.
    let display = if app.input.is_empty() && !pending {
        String::new()
    } else {
        app.input.clone()
    };
    let para = Paragraph::new(display.as_str())
        .style(if pending { theme.approval } else { theme.input })
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);

    // Cursor position accounting for multi-line wrap.
    let cursor_text = &app.input[..app.cursor.min(app.input.len())];
    let (cx, cy) = cursor_pos(cursor_text, inner.width.max(1) as usize);
    let cursor_x = (inner.x + cx as u16).min(inner.x + inner.width.saturating_sub(1));
    let cursor_y = (inner.y + cy as u16).min(inner.y + inner.height.saturating_sub(1));
    // Stashed on App rather than `frame.set_cursor_position` — the custom
    // draw wrapper (`draw_dedup_cursor`) reads this to decide whether the
    // cursor escape actually needs re-emitting this frame. See the
    // `desired_cursor` field doc comment for why.
    app.desired_cursor = Some((cursor_x, cursor_y));
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
    let w = area.width.clamp(40, 64);
    let h = area.height.clamp(14, 20);
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
        .border_style(theme.input_border)
        .title(Span::styled(" help ", theme.brand));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = vec![
        Line::from(Span::styled("Keys", theme.heading)),
        Line::from(Span::styled(
            "  enter            send message",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  shift/alt+enter  newline (or ctrl-j)",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ↑ / ↓            input history",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  pgup / pgdn      scroll chat",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  mouse wheel      scroll chat",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  esc              cancel run / clear input",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-c           cancel (or quit if idle)",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-d           quit",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-l           clear screen",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ctrl-u           clear input",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  ?                this help",
            theme.assistant_text,
        )),
        Line::from(""),
        Line::from(Span::styled("Commands", theme.heading)),
        Line::from(Span::styled("  /help  /clear  /quit", theme.assistant_text)),
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
                app.dirty = true;
            }
        }
        AgentEvent::MessageUpdate { message } => {
            if app.live.is_none() {
                return;
            }
            if app.last_live_refresh.elapsed() < std::time::Duration::from_millis(80) {
                // Always keep latest content even if we skip a paint.
                let thinking = extract_thinking(&message);
                let text = message.text();
                app.live = Some((thinking, text));
                return;
            }
            app.last_live_refresh = std::time::Instant::now();
            let thinking = extract_thinking(&message);
            let text = message.text();
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
            app.push(ChatItem::ToolStart {
                name: tool_name,
                summary,
            });
        }
        AgentEvent::ToolExecutionEnd { result, .. } => {
            let text: String = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            let preview: String = text.lines().take(8).collect::<Vec<_>>().join("\n");
            if !preview.is_empty() || result.is_error {
                app.push(ChatItem::ToolEnd {
                    preview: if preview.is_empty() {
                        "(error)".into()
                    } else {
                        preview
                    },
                    is_error: result.is_error,
                });
            }
        }
        AgentEvent::CompactionStart { .. } => {
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
            app.set_status("thinking");
        }
        AgentEvent::TurnEnd { .. } => {
            app.set_status("running");
        }
        AgentEvent::AgentStart => {
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
        let lines = ChatItem::User("hello".into()).render(&theme, 80);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("|");
        assert!(flat.contains("you"));
        assert!(flat.contains("hello"));
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
    fn tool_icon_known() {
        assert_eq!(tool_icon("bash"), "▸");
        assert_eq!(tool_icon("read"), "◉");
        assert_eq!(tool_icon("mystery"), "○");
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
            approval_mode: "auto".into(),
            cwd_label: ".".into(),
            usage_summary: String::new(),
            pending_approval: Arc::new(Mutex::new(None)),
            approval_answer: Arc::new(std::sync::mpsc::channel().0),
            cancel: Arc::new(Mutex::new(tokio_util::sync::CancellationToken::new())),
            show_help: false,
            status_msg: String::new(),
            should_quit: false,
            item_caches: Vec::new(),
            cache_width: 0,
            total_rows: 0,
            last_draw_width: 0,
            desired_cursor: None,
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
