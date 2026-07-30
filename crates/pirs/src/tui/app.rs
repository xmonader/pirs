use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pirs_agent::{Agent, AgentTool};
use ratatui::text::Line;

use crate::approval::ApprovalMode;
use crate::session_stats::{self, SessionClock};

use super::chat::ChatItem;
use super::journey::*;
use super::model_picker::ModelPicker;
use super::tools::*;

// ── App state ───────────────────────────────────────────────────────────────

/// One committed chat item's wrapped-row cache. `rows: None` means the exact
/// rows aren't currently known at the active width — either the item was
/// just pushed and has never been measured, or its previous measurement was
/// invalidated by a resize, or it was evicted for being far off-screen — in
/// all three cases `row_count` still holds a best-known estimate (possibly
/// stale) so scroll-position math stays correct without needing the exact
/// content.
pub(super) struct ItemCache {
    pub(super) rows: Option<Vec<Line<'static>>>,
    pub(super) row_count: usize,
}

pub struct TuiOptions {
    pub agent: Agent,
    pub host: Option<Arc<pirs_rhai::ExtensionHost>>,
    /// Session JSONL path (shown in status / welcome).
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
pub(super) struct SessionControls {
    pub(super) strategy: Option<String>,
    pub(super) plan_model: Option<String>,
}

pub(super) struct App {
    pub(super) items: Vec<ChatItem>,
    /// Live streaming assistant content (thinking + text), not yet committed.
    pub(super) live: Option<(String, String)>,
    pub(super) input: String,
    /// Byte index of the cursor inside `input`.
    pub(super) cursor: usize,
    pub(super) history: Vec<String>,
    pub(super) history_idx: Option<usize>,
    pub(super) history_draft: String,
    pub(super) running: bool,
    pub(super) tick: u64,
    pub(super) dirty: bool,
    pub(super) last_live_refresh: std::time::Instant,
    pub(super) steer_queue: Arc<Mutex<Vec<String>>>,
    /// Rows scrolled up from the bottom (0 = pinned).
    pub(super) scroll: u16,
    pub(super) viewport_height: u16,
    pub(super) model: String,
    pub(super) plan_model: Option<String>,
    pub(super) strategy: Option<String>,
    pub(super) model_aliases: Vec<String>,
    pub(super) approval_mode: String,
    /// Live session JSONL path (from CLI session_path).
    pub(super) session_path: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) cwd_label: String,
    pub(super) usage_summary: String,
    pub(super) pending_approval: Arc<Mutex<Option<String>>>,
    pub(super) approval_answer: Arc<std::sync::mpsc::Sender<String>>,
    /// When the current approval prompt was shown (grace period for Enter).
    pub(super) approval_opened_at: Option<std::time::Instant>,
    pub(super) cancel: pirs_agent::agent::CancelSlot,
    pub(super) show_help: bool,
    /// Fuzzy model picker overlay (`/models`, `/model` empty).
    pub(super) model_picker: Option<ModelPicker>,
    pub(super) status_msg: String,
    /// Human activity label for turn-status ("thinking", "bash", …).
    pub(super) last_activity: String,
    pub(super) turn_started_at: Option<std::time::Instant>,
    pub(super) thinking_expanded: bool,
    /// Selected row in the slash completion popup (0-based into filtered list).
    pub(super) slash_sel: usize,
    /// Extension slash commands `(name, description)` without leading `/`
    /// (from `ExtensionHost::commands`). Merged into the Tab-complete list.
    pub(super) ext_slash: Vec<(String, String)>,
    /// Session started as first-run onboarding (for /tour re-show).
    pub(super) first_run_session: bool,
    pub(super) should_quit: bool,
    /// One entry per `items[i]`: wrapped physical rows for that item, when
    /// known — virtualized so a long conversation with large tool outputs
    /// doesn't pay to re-wrap the *entire* history every time a single new
    /// item is pushed. See `ItemCache` and `draw_chat`'s three-pass
    /// measure/clamp/paint for how entries near the viewport stay exactly
    /// measured while everything else keeps only a row-count estimate.
    pub(super) item_caches: Vec<ItemCache>,
    pub(super) cache_width: usize,
    pub(super) total_rows: usize,
    pub(super) last_draw_width: usize,
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
    pub(super) desired_cursor: Option<(u16, u16)>,
    /// Session wall / agent-busy timers for exit stats.
    pub(super) clock: SessionClock,
}

impl App {
    pub(super) fn push(&mut self, item: ChatItem) {
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

    pub(super) fn notice(&mut self, text: impl Into<String>) {
        self.push(ChatItem::Notice(text.into()));
    }

    pub(super) fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = msg.into();
        self.dirty = true;
    }

    /// Clear transcript + caches (Ctrl-L / `/clear`). Keeps length invariant.
    pub(super) fn clear_chat(&mut self) {
        self.items.clear();
        self.item_caches.clear();
        self.live = None;
        self.scroll = 0;
        self.total_rows = 0;
        self.cache_width = 0;
        self.notice("screen cleared");
    }

    pub(super) fn invalidate_item(&mut self, idx: usize) {
        if let Some(c) = self.item_caches.get_mut(idx) {
            c.rows = None;
            // Keep a modest estimate so scroll doesn't jump to zero height
            // before the next measure pass; draw_chat remeasures when near.
            c.row_count = c.row_count.max(1);
        }
        self.dirty = true;
    }

    /// Toggle collapsed thinking on all assistant messages + live stream.
    pub(super) fn toggle_thoughts(&mut self) {
        self.thinking_expanded = !self.thinking_expanded;
        for i in 0..self.items.len() {
            if let ChatItem::Assistant { thinking, .. } = &self.items[i] {
                if thinking.trim().is_empty() {
                    continue;
                }
                let estimate = if self.thinking_expanded {
                    // Rough pre-measure so scroll math doesn't clamp before paint.
                    thinking
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .count()
                        .min(80)
                        + 6
                } else {
                    4
                };
                if let Some(c) = self.item_caches.get_mut(i) {
                    c.rows = None;
                    c.row_count = estimate.max(1);
                }
            }
        }
        self.dirty = true;
        let n = self
            .items
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    ChatItem::Assistant { thinking, .. } if !thinking.trim().is_empty()
                )
            })
            .count();
        self.set_status(if self.thinking_expanded {
            format!("thoughts shown ({n} msg) · t / ctrl-o / /thoughts to hide")
        } else {
            format!("thoughts hidden ({n} msg) · t / ctrl-o / /thoughts to show")
        });
    }

    pub(super) fn start_tool(&mut self, name: String, summary: String) {
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

    pub(super) fn finish_tool(&mut self, name: &str, preview: String, is_error: bool) {
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
    pub(super) fn collapse_trailing_quiet_tools(&mut self) {
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

    pub(super) fn toggle_last_tool_expand(&mut self) {
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

    pub(super) fn apply_starter(&mut self, idx: usize) {
        if let Some(p) = STARTER_PROMPTS.get(idx) {
            self.input = (*p).to_string();
            self.cursor = self.input.len();
            self.history_idx = None;
            self.dirty = true;
            self.set_status(format!("starter {} — press Enter to send", idx + 1));
        }
    }

    /// Live hybrid pin snapshot for session-end reporting (plan-model + strategy).
    /// Built from app state so `/plan-model` / `/strategy` updates stay on the
    /// same path as CLI-seeded pins — never hardcode empty at print sites.
    pub(super) fn report_pins(&self) -> session_stats::ReportPins {
        session_stats::ReportPins::new(self.plan_model.clone(), self.strategy.clone())
    }

    pub(super) fn push_tour_welcome(&mut self) {
        self.push(ChatItem::Welcome {
            model: self.model.clone(),
            plan_model: self.plan_model.clone(),
            strategy: self.strategy.clone(),
            approval: self.approval_mode.clone(),
            cwd: self.cwd_label.clone(),
            first_run: true,
        });
    }

    pub(super) fn mark_running(&mut self, activity: impl Into<String>) {
        self.running = true;
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(std::time::Instant::now());
        }
        self.last_activity = activity.into();
        self.dirty = true;
    }

    pub(super) fn mark_idle(&mut self) {
        self.running = false;
        self.turn_started_at = None;
        self.last_activity.clear();
        self.set_status(String::new());
    }
}
