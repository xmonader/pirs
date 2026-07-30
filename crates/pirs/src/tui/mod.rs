//! Interactive terminal UI for pirs (`--mode tui`).
//!
//! Layout (top → bottom), polished against grok-build / mistral-vibe / qwen-code:
//!   header · chat · turn-status · input
//!
//! Split modules: app, chat, draw, events, input, slash_exec, terminal (+ existing theme/slash/tools/...).

use std::sync::{Arc, Mutex};

use crossterm::event::Event;
use pirs_agent::{Agent, AgentEvent};
use pirs_ai::Message;

use crate::session_stats::{self, SessionClock};

// Test-only / re-export surface for tests_all.
#[cfg(test)]
#[allow(unused_imports)]
use {
    crate::approval::ApprovalMode,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    ratatui::Terminal,
    std::path::PathBuf,
};

mod journey;
mod model_picker;
mod slash;
mod theme;
mod tools;

mod app;
mod chat;
mod draw;
mod events;
mod input;
mod layout_util;
mod slash_exec;
mod terminal;

pub use app::TuiOptions;

// Re-exports for submodule + tests_all (`use super::*`). Unused in the non-test binary target.
#[allow(unused_imports)]
use {
    app::*,
    chat::*,
    draw::*,
    events::*,
    input::*,
    journey::*,
    layout_util::*,
    model_picker::{ModelPicker, ModelPickerTarget},
    slash::*,
    slash_exec::*,
    terminal::*,
    theme::*,
    tools::*,
};

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
    // Prefer live gate mode when present (keeps ApprovalGate::mode on the product path).
    let approval_name = opts
        .approval_gate
        .as_ref()
        .map(|g| g.mode().name().to_string())
        .unwrap_or_else(|| opts.approval_mode.name().to_string());
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
        session_path: opts.session_path.clone(),
        cwd: opts.cwd.clone(),
        cwd_label,
        usage_summary: String::new(),
        pending_approval,
        approval_answer: approval_answer_rx,
        approval_opened_at: None,
        cancel,
        show_help: false,
        model_picker: None,
        status_msg: String::new(),
        last_activity: String::new(),
        turn_started_at: None,
        // Collapsed by default — raw CoT flood next to the composer felt like junk.
        // Expand with `t` / ctrl-o.
        thinking_expanded: false,
        slash_sel: 0,
        ext_slash: opts.host.as_ref().map(|h| h.commands()).unwrap_or_default(),
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
    let (shell_tx, mut shell_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String, bool)>();
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
                    opts.host.as_ref(),
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
                // Spinner / elapsed / stream caret: repaint ~5/s, not every 50ms.
                // Full-frame redraws at 20Hz made the compose box flicker.
                if app.running || app.live.is_some() {
                    app.tick = app.tick.wrapping_add(1);
                    if app.tick.is_multiple_of(4) {
                        app.dirty = true;
                    }
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

    // Deny any pending approval and cancel the agent *before* restore/stats so
    // we never deadlock forever on agent.lock while the worker holds it in an
    // approval recv (review C-5).
    tui_prepare_exit(&app, &agent).await;

    // Explicit restore before Drop (Drop is best-effort).
    restore_terminal()?;
    drop(_guard);

    // Session stats after the alternate screen is gone (qwen-code-style exit summary).
    {
        app.clock.agent_end();
        let report = {
            // try_lock: if still busy after cancel, skip stats rather than hang.
            match agent.try_lock() {
                Ok(a) => a.usage_report(),
                Err(_) => {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), agent.lock())
                        .await
                    {
                        Ok(a) => a.usage_report(),
                        Err(_) => pirs_agent::usage::UsageReport::default(),
                    }
                }
            }
        };
        let pins = app.report_pins();
        session_stats::print_session_stats_pins(&app.clock, &report, &app.model, &pins);
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

/// Cancel in-flight work and deny pending approvals so quit cannot deadlock
/// on `agent.lock()` (review C-5).
async fn tui_prepare_exit(app: &App, agent: &Arc<tokio::sync::Mutex<Agent>>) {
    // Answer any pending approval with deny so the worker unblocks.
    {
        let mut pending = app.pending_approval.lock().unwrap();
        if pending.is_some() {
            *pending = None;
            let _ = app.approval_answer.send("n".into());
        }
    }
    // Request cancel on the live turn.
    app.cancel.lock().unwrap().cancel();
    // Best-effort: also cancel via agent handle if still lockable quickly.
    if let Ok(a) = agent.try_lock() {
        let _ = a; // cancel already fired via shared CancelSlot
    }
}

#[cfg(test)]
#[path = "tests_all.rs"]
mod tests;
