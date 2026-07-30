use std::sync::{Arc, Mutex};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::ExecutableCommand;
use ratatui::Terminal;

use super::chat::ChatItem;

// ── Terminal lifecycle ──────────────────────────────────────────────────────

pub(super) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// Whether to capture the mouse (scroll wheel). Default **off** so the
/// terminal emulator keeps native text selection + copy (click-drag / shift).
/// Set `PIRS_TUI_MOUSE=1` to re-enable wheel scroll via the app.
pub(super) fn mouse_capture_enabled() -> bool {
    matches!(
        std::env::var("PIRS_TUI_MOUSE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// Puts the real terminal into raw/alt-screen mode (optional mouse capture),
/// but the returned `Terminal` renders into an in-memory buffer, not real
/// stdout — see `TuiWriter` for why. Terminal-size and cursor-position queries
/// still hit the real tty regardless of what the backend's writer is.
pub(super) fn setup_terminal(
) -> anyhow::Result<Terminal<ratatui::backend::CrosstermBackend<Vec<u8>>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(crossterm::terminal::EnterAlternateScreen)?;
    // Default: leave mouse free so users can select + copy from chat output.
    // Wheel scroll still works via keyboard (g/G, etc.) when capture is off.
    if mouse_capture_enabled() {
        stdout.execute(EnableMouseCapture)?;
    }
    let backend = ratatui::backend::CrosstermBackend::new(Vec::new());
    Ok(Terminal::new(backend)?)
}

/// A single-slot mailbox that always holds only the most recently pushed
/// value: `push` replaces — never queues behind — anything not yet taken.
/// This is the backpressure gate itself, factored out from `TuiWriter` so
/// its coalescing behavior is unit-testable without a real terminal or
/// background thread.
pub(super) struct LatestSlot<T> {
    state: Mutex<LatestSlotState<T>>,
    cvar: std::sync::Condvar,
}

pub(super) struct LatestSlotState<T> {
    value: Option<T>,
    closed: bool,
}

impl<T> LatestSlot<T> {
    pub(super) fn new() -> Self {
        LatestSlot {
            state: Mutex::new(LatestSlotState {
                value: None,
                closed: false,
            }),
            cvar: std::sync::Condvar::new(),
        }
    }

    /// Replace-semantics push. Production frame writes go through
    /// [`LatestSlot::push_coalesce`] instead (see it for why replacing drops
    /// ratatui diff cells); this remains only to exercise the generic
    /// cvar/close mechanics in tests.
    #[cfg(test)]
    pub(super) fn push(&self, value: T) {
        let mut guard = self.state.lock().unwrap();
        guard.value = Some(value);
        self.cvar.notify_one();
    }

    /// Blocks until a value is available, returning it immediately if one is
    /// already pending. Returns `None` once `close` has been called and
    /// nothing is left to take — the signal for the consumer to stop.
    pub(super) fn take_blocking(&self) -> Option<T> {
        let mut guard = self.state.lock().unwrap();
        while guard.value.is_none() && !guard.closed {
            guard = self.cvar.wait(guard).unwrap();
        }
        guard.value.take()
    }

    pub(super) fn close(&self) {
        let mut guard = self.state.lock().unwrap();
        guard.closed = true;
        self.cvar.notify_one();
    }
}

impl LatestSlot<Vec<u8>> {
    /// Frame-delta variant of [`LatestSlot::push`]: *appends* to any
    /// not-yet-written pending bytes instead of replacing them.
    ///
    /// The rendered frames handed to the writer are ratatui *incremental
    /// diffs* (only the cells that changed since the previous frame, emitted
    /// with absolute cursor moves), not full repaints. Replacing a pending
    /// diff would silently drop the cells it painted -- and ratatui's double
    /// buffer already believes those cells are on screen, so it never re-emits
    /// them. Under heavy token streaming the writer thread is frequently mid-
    /// flush, so this dropped exactly the deltas that paint keystrokes typed
    /// during a response, garbling the input line.
    ///
    /// Concatenating consecutive diffs reproduces byte-for-byte what a
    /// keeping-up writer would have written, so the screen still converges on
    /// the latest state with nothing lost. The buffer stays small: deltas are
    /// tiny and the writer coalesces them into a single `write_all`, so there
    /// is still no backpressure on the render loop.
    pub(super) fn push_coalesce(&self, mut bytes: Vec<u8>) {
        let mut guard = self.state.lock().unwrap();
        match guard.value.as_mut() {
            Some(pending) => pending.append(&mut bytes),
            None => guard.value = Some(bytes),
        }
        self.cvar.notify_one();
    }
}

/// Decouples the actual terminal write (a blocking OS syscall that can stall
/// under a slow pty/tmux/SSH pipe) from the async event loop that computes
/// frames. The loop renders each frame into an in-memory
/// `CrosstermBackend<Vec<u8>>` (cheap, CPU-only) and hands the resulting
/// bytes to this writer, which owns real stdout on a dedicated OS thread.
/// Pending frames are coalesced (via `LatestSlot::push_coalesce`): if the
/// writer thread is still flushing a previous frame when a new one is
/// computed, the new delta is appended to the pending bytes rather than
/// queued as a separate flush, so heavy token streaming (many redraws in
/// quick succession) never backs up waiting on terminal I/O — the writer
/// catches up in a single write, and the screen converges on the latest
/// state. (Appending, not replacing: the frames are ratatui incremental
/// diffs, so a dropped delta would lose the cells it painted for good.)
pub(super) struct TuiWriter {
    slot: Arc<LatestSlot<Vec<u8>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TuiWriter {
    pub(super) fn spawn() -> Self {
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

    /// Hands off a rendered frame's delta bytes to the writer thread. If a
    /// previous frame is still pending (writer mid-flush), the new delta is
    /// *appended* to it rather than replacing it -- see
    /// [`LatestSlot::push_coalesce`] for why replacing corrupts the screen
    /// with diff-based frames. Never blocks: coalescing keeps the pending
    /// buffer to a single write, so this is still the backpressure gate, just
    /// expressed as "collapse pending deltas into one write" instead of "drop
    /// all but the newest".
    pub(super) fn push(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.slot.push_coalesce(bytes);
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
    pub(super) fn shutdown(mut self) {
        self.close_and_join();
    }

    pub(super) fn close_and_join(&mut self) {
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

pub(super) fn restore_terminal() -> anyhow::Result<()> {
    // Safe even if mouse was never captured.
    let _ = std::io::stdout().execute(DisableMouseCapture);
    let _ = std::io::stdout().execute(crossterm::terminal::LeaveAlternateScreen);
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// Last assistant reply text from chat history (for `/copy`).
pub(super) fn last_assistant_text(items: &[ChatItem]) -> Option<String> {
    items.iter().rev().find_map(|it| match it {
        ChatItem::Assistant { text, error, .. } => {
            let mut s = text.trim().to_string();
            if let Some(e) = error {
                if !e.trim().is_empty() {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str(e.trim());
                }
            }
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        _ => None,
    })
}

/// Copy text to the system clipboard via common CLI tools (no extra crate).
/// Tries: `wl-copy`, `xclip`, `xsel`, `pbcopy` (macOS), `clip.exe` (WSL).
pub(super) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
        ("clip.exe", &[]),
    ];
    let mut last_err = String::from("no clipboard helper found");
    for (bin, args) in candidates {
        let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                last_err = format!("{bin}: failed to write stdin");
                let _ = child.kill();
                continue;
            }
        }
        match child.wait() {
            Ok(st) if st.success() => return Ok(()),
            Ok(st) => last_err = format!("{bin}: exit {st}"),
            Err(e) => last_err = format!("{bin}: {e}"),
        }
    }
    Err(format!(
        "{last_err} (install wl-copy, xclip, xsel, or pbcopy)"
    ))
}

pub(super) fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        prev(info);
    }));
}
