use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::block::Padding;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;

use super::app::App;
use super::chat::{
    render_markdown, render_thinking_live, ChatItem,
};
use super::model_picker::{draw_model_picker, ModelPicker};
use super::slash::*;
use super::theme::*;
use super::layout_util::*;
use super::tools::*;

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
pub(super) fn draw_with_cursor_dedup<B, F>(
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

    // When the cursor is hidden, re-emit Hide on *every* frame. Ratatui
    // paints cell diffs with absolute MoveTo; the physical caret then sits
    // on the last painted cell (often the status-bar spinner, right before
    // "thinking") until the next Show/Hide. Deduping Hide left it visible
    // and "flickering" on the status line as that row redrew.
    // When shown, still only MoveTo on change so idle blink isn't reset.
    match desired {
        None => {
            terminal.hide_cursor()?;
            *last_cursor = None;
        }
        Some(pos) if desired != *last_cursor => {
            terminal.show_cursor()?;
            terminal.set_cursor_position(pos)?;
            *last_cursor = desired;
        }
        Some(_) => {}
    }

    terminal.swap_buffers();
    terminal.backend_mut().flush()?;
    Ok(())
}

/// Renders one TUI frame with the cursor-blink-preserving dedup wrapper —
/// see `draw_with_cursor_dedup` and `App::desired_cursor` for why this
/// exists instead of a plain `terminal.draw(|frame| draw_ui(frame, app))`.
pub(super) fn draw_dedup_cursor<B: ratatui::backend::Backend>(
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

pub(super) fn draw_ui(frame: &mut ratatui::Frame, app: &mut App) {
    let theme = Theme::default_dark();

    // Leave the last row unused: writing the bottom-right corner cell scrolls
    // the terminal and corrupts every subsequent frame.
    let full = frame.area();
    let area = Rect {
        height: full.height.saturating_sub(1),
        ..full
    };

    // Roomier chrome: taller compose (min 2 text rows), 1-row gutters between
    // header/chat/status so regions don't weld together.
    let input_lines = app.input.lines().count().clamp(2, 8) as u16;
    let input_h = input_lines + 2; // borders
    let pending = app.pending_approval.lock().unwrap().is_some();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // gutter
            Constraint::Min(4),    // chat
            Constraint::Length(1), // gutter
            Constraint::Length(1), // turn-status
            Constraint::Length(1), // gutter before composer
            Constraint::Length(input_h),
        ])
        .split(area);

    draw_header(frame, chunks[0], app, &theme);
    // chunks[1] = air
    draw_chat(frame, chunks[2], app, &theme);
    // chunks[3] = air
    draw_status(frame, chunks[4], app, &theme);
    // chunks[5] = air
    draw_input(frame, chunks[6], app, &theme);

    if slash_completing(&app.input) && !pending && app.model_picker.is_none() {
        draw_slash_popup(frame, chunks[6], app, &theme);
    }
    if pending {
        draw_approval_overlay(frame, area, app, &theme);
    }
    if app.show_help {
        draw_help_overlay(frame, area, &theme);
    }
    if let Some(picker) = &app.model_picker {
        draw_model_picker(frame, area, picker, &theme);
    }
}

pub(super) fn draw_slash_popup(frame: &mut ratatui::Frame, input_area: Rect, app: &App, theme: &Theme) {
    let matches = slash_filter(app.input.trim(), &app.ext_slash);
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

pub(super) fn draw_header(frame: &mut ratatui::Frame, area: Rect, app: &App, theme: &Theme) {
    // Thin identity chrome; token usage lives on the turn-status row (qwen footer pattern).
    let mode_style = composer_mode_style(theme, &app.approval_mode, false, false);
    let mut left = vec![
        Span::styled("  pirs ", theme.brand),
        Span::styled(" · ", theme.dim),
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
    // Session JSONL stem (consumes TuiOptions.session_path / App.session_path).
    if let Some(stem) = app.session_path.file_stem().and_then(|s| s.to_str()) {
        left.push(Span::styled(" sess:", theme.dim));
        left.push(Span::styled(stem.to_string(), theme.dim));
    }
    left.push(Span::styled("  ", theme.dim));
    left.push(Span::styled(
        format!("● {}", app.approval_mode),
        mode_style,
    ));
    left.push(Span::styled("  ", theme.dim));
    let ctx = pirs_tools::current_work_context();
    if ctx.roots.len() > 1 {
        let names: Vec<&str> = ctx.names();
        left.push(Span::styled(
            format!("ctx:{}", names.join("+")),
            theme.accent,
        ));
        left.push(Span::styled("  ", theme.dim));
    }
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
pub(super) const VIRTUALIZE_MARGIN_ROWS: usize = 200;

pub(super) fn draw_chat(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme.border)
        .title(Span::styled(" chat ", theme.dim))
        // Side padding so transcript doesn't hug the terminal edge.
        .padding(Padding::new(1, 1, 0, 0));
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
        let phase = if !thinking.trim().is_empty() && text.trim().is_empty() {
            "  · thinking"
        } else if !thinking.trim().is_empty() {
            "  · answering"
        } else {
            "  · streaming"
        };
        let mut logical: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  │ ", theme.assistant_label),
                Span::styled("assistant", theme.assistant_label),
                Span::styled(phase.to_string(), theme.dim),
            ]),
        ];
        if !thinking.trim().is_empty() {
            logical.extend(render_thinking_live(
                thinking,
                theme,
                app.thinking_expanded,
            ));
        }
        if !text.trim().is_empty() {
            logical.extend(render_markdown(text, theme, width.saturating_sub(4)));
        }
        // Blinking caret only while answer text is streaming. During pure
        // thinking it sat under the status line and fought the real input
        // cursor (looked like a caret flicking start/end of the thinking row).
        if !text.trim().is_empty() {
            let caret = if (app.tick / 4).is_multiple_of(2) {
                "▌"
            } else {
                " "
            };
            logical.push(Line::from(Span::styled(
                format!("    {caret}"),
                theme.accent,
            )));
        }
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

pub(super) fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    // Spinner only for tool work — pure "thinking" uses a static glyph so the
    // cell before the label doesn't thrash every tick (looked like a caret).
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
        let activity = clean_activity_label(app);
        let is_thinking = activity == "thinking" || activity == "writing";
        // Fixed-width prefix (3 cols) so label never shifts.
        if is_thinking {
            left.push(Span::styled("  · ", theme.accent));
        } else {
            let spin = FRAMES[((app.tick / 4) % 10) as usize];
            left.push(Span::styled(format!(" {spin} "), theme.accent));
        }
        // Pad activity to fixed width so "thinking"/"writing"/"running shell"
        // don't shove the elapsed time (and look like end-of-label flicker).
        let label = format!("{activity:<14}");
        left.push(Span::styled(label, theme.status));
        if let Some(start) = app.turn_started_at {
            // Fixed-width elapsed (e.g. "  5s" / "1m05s") so the right side stays put.
            let elapsed = format_elapsed(start.elapsed().as_secs());
            left.push(Span::styled(format!(" · {elapsed}"), theme.dim));
        }
        right.push(Span::styled(" esc cancel ", theme.dim));
    } else {
        left.push(Span::styled("  · ", theme.dim));
        left.push(Span::styled("ready", theme.status));
        // Show autonomy badge so the bar matches the composer title.
        let auto = match pirs_tools::live_permission_mode() {
            pirs_tools::PermissionMode::ReadOnly => "plan",
            pirs_tools::PermissionMode::WorkspaceWrite => "edit",
            pirs_tools::PermissionMode::DangerFullAccess => "full",
        };
        left.push(Span::styled(format!(" · {auto}"), theme.dim));
        if !app.status_msg.is_empty() {
            left.push(Span::styled(format!(" · {}", app.status_msg), theme.dim));
        }
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

/// Map noisy internal activity strings to short status-bar verbs.
pub(super) fn clean_activity_label(app: &App) -> String {
    if let Some((thinking, text)) = &app.live {
        if !thinking.trim().is_empty() && text.trim().is_empty() {
            return "thinking".into();
        }
        if !text.trim().is_empty() {
            return "writing".into();
        }
    }
    let raw = if !app.last_activity.is_empty() {
        app.last_activity.as_str()
    } else if !app.status_msg.is_empty() {
        app.status_msg.as_str()
    } else {
        "working"
    };
    let lower = raw.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("shell") {
        return "running shell".into();
    }
    if lower.contains("read") {
        return "reading".into();
    }
    if lower.contains("edit") || lower.contains("write") {
        return "editing".into();
    }
    if lower.contains("think") {
        return "thinking".into();
    }
    if lower.contains("steer") {
        return "steering".into();
    }
    // Keep short; strip tool-call spam.
    if raw.chars().count() > 28 {
        let head: String = raw.chars().take(27).collect();
        return format!("{head}…");
    }
    raw.to_string()
}

pub(super) fn draw_input(frame: &mut ratatui::Frame, area: Rect, app: &mut App, theme: &Theme) {
    let pending = app.pending_approval.lock().unwrap().is_some();
    let border_style = composer_mode_style(theme, &app.approval_mode, app.running, pending);
    // Title = short mode badge (never mislabel default auto as "yolo").
    let title = composer_title(&app.approval_mode, app.running, pending);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, theme.dim))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Empty compose: blank. Hints live in the status bar, not inside the box.
    let (display, style) = if app.input.is_empty() && !pending {
        (String::new(), theme.input)
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

    // Cursor: hide while the agent runs with an empty composer so the real
    // terminal caret doesn't sit in the input box and thrash as the live
    // thinking pane above redraws every token. Show again as soon as the
    // user types to steer (or when idle).
    if app.running && app.input.is_empty() && !pending {
        app.desired_cursor = None;
        return;
    }
    // Cursor position accounting for multi-line wrap.
    let cursor_text = if app.input.is_empty() {
        ""
    } else {
        &app.input[..app.cursor.min(app.input.len())]
    };
    let (cx, cy) = cursor_pos(cursor_text, inner.width.max(1) as usize);
    let cursor_x = (inner.x + cx as u16).min(inner.x + inner.width.saturating_sub(1));
    let cursor_y = (inner.y + cy as u16).min(inner.y + inner.height.saturating_sub(1));
    app.desired_cursor = Some((cursor_x, cursor_y));
}

pub(super) fn draw_approval_overlay(frame: &mut ratatui::Frame, area: Rect, app: &App, theme: &Theme) {
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

pub(super) fn cursor_pos(text_before_cursor: &str, width: usize) -> (usize, usize) {
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

pub(super) fn draw_help_overlay(frame: &mut ratatui::Frame, area: Rect, theme: &Theme) {
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
            "  t / ctrl-o / /thoughts   expand/collapse thoughts",
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
            "  /model /models  fuzzy pick · /models refresh",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /backends /key /backend add /setup",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /tour /plan-model /strategy /goal /stats /copy",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /status /features     runtime + packs + caps",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /autonomy plan|edit|full  ·  /plan /edit /act /yolo",
            theme.assistant_text,
        )),
        Line::from(Span::styled(
            "  /permission /profile (legacy) · /checkpoint",
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
            "  select+copy: drag in terminal (mouse free by default)",
            theme.dim,
        )),
        Line::from(Span::styled(
            "  /copy last reply · PIRS_TUI_MOUSE=1 for wheel scroll",
            theme.dim,
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
