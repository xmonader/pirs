use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use pirs_agent::Agent;
use pirs_ai::Message;
use std::sync::{Arc, Mutex};

use super::app::{App, SessionControls};
use super::chat::ChatItem;
use super::model_picker::{ModelPicker, ModelPickerTarget};
use super::slash::*;
use super::slash_exec::{
    attach_image_to_agent, handle_model_picker_key, handle_slash_command,
};
use super::theme::*;

// ── Input handling ──────────────────────────────────────────────────────────

/// Returns true if the app should quit.
pub(super) fn handle_key(
    app: &mut App,
    key: KeyEvent,
    prompt_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    shell_tx: &tokio::sync::mpsc::UnboundedSender<(String, String, bool)>,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
    host: Option<&Arc<pirs_rhai::ExtensionHost>>,
) -> bool {
    // Fuzzy model picker takes over the keyboard while open.
    if app.model_picker.is_some() {
        return handle_model_picker_key(app, key, agent, controls);
    }

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
        (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.clear_chat();
        }
        // Thoughts: ctrl-o always; bare `t` when compose is empty (easy to hit).
        (KeyCode::Char('o'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.toggle_thoughts();
        }
        (KeyCode::Char('t'), KeyModifiers::NONE) if app.input.is_empty() => {
            app.toggle_thoughts();
        }
        (KeyCode::Char('T'), KeyModifiers::SHIFT) if app.input.is_empty() => {
            app.toggle_thoughts();
        }
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
            delete_word_before_cursor(app);
        }
        (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => {
            app.cursor = 0;
        }
        (KeyCode::Char('e'), m) if m.contains(KeyModifiers::CONTROL) => {
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
                let matches = slash_filter(app.input.trim(), &app.ext_slash);
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
                            .is_some_and(|c| !app.input.trim().eq_ignore_ascii_case(&c.name))
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
                            submit_input(app, prompt_tx, shell_tx, agent, controls, host);
                        }
                        return false;
                    }
                }
            }
            submit_input(app, prompt_tx, shell_tx, agent, controls, host);
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
            let n = slash_filter(app.input.trim(), &app.ext_slash).len();
            if n > 0 {
                app.slash_sel = app.slash_sel.saturating_add(n - 1) % n;
            }
        }
        (KeyCode::Down, _) if slash_completing(&app.input) => {
            let n = slash_filter(app.input.trim(), &app.ext_slash).len();
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

pub(super) fn apply_slash_completion(app: &mut App) {
    let matches = slash_filter(app.input.trim(), &app.ext_slash);
    if matches.is_empty() {
        return;
    }
    let idx = app.slash_sel.min(matches.len() - 1);
    let name = matches[idx].name.clone();
    app.input = format!("{name} ");
    app.cursor = app.input.len();
    app.slash_sel = 0;
    app.history_idx = None;
    app.dirty = true;
}

pub(super) fn delete_word_before_cursor(app: &mut App) {
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

pub(super) fn handle_mouse(app: &mut App, mouse: MouseEvent) {
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

pub(super) fn max_scroll(app: &App) -> u16 {
    // Real wrapped-row total from the last frame; draw re-clamps every frame.
    app.total_rows
        .saturating_sub(app.viewport_height as usize)
        .min(u16::MAX as usize) as u16
}

pub(super) fn insert_at_cursor(app: &mut App, c: char) {
    app.input.insert(app.cursor, c);
    app.cursor += c.len_utf8();
    app.history_idx = None;
}

pub(super) fn delete_before_cursor(app: &mut App) {
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

pub(super) fn delete_after_cursor(app: &mut App) {
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

pub(super) fn move_cursor_left(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    app.cursor = app.input[..app.cursor]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0);
}

pub(super) fn move_cursor_right(app: &mut App) {
    if app.cursor >= app.input.len() {
        return;
    }
    app.cursor = app.input[app.cursor..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| app.cursor + i)
        .unwrap_or(app.input.len());
}

pub(super) fn history_up(app: &mut App) {
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

pub(super) fn history_down(app: &mut App) {
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

pub(super) fn submit_input(
    app: &mut App,
    prompt_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    shell_tx: &tokio::sync::mpsc::UnboundedSender<(String, String, bool)>,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
    host: Option<&Arc<pirs_rhai::ExtensionHost>>,
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

    // Slash commands: model / plan-model / strategy / extension cmds (e.g. /goal).
    if text.starts_with('/') {
        handle_slash_command(app, agent, controls, &text, host);
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
pub(super) fn run_shell_command(cwd: &std::path::Path, cmd: &str) -> String {
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

