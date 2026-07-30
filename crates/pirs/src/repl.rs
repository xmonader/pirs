//! Interactive rustyline REPL and slash-like colon commands.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use pirs_agent::{Agent, AgentTool};
use pirs_ai::Message;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::printer::Printer;
use crate::turn::run_turn;

pub async fn repl(
    agent: &mut Agent,
    printer: &Arc<Printer>,
    session_path: &std::sync::Arc<std::sync::Mutex<PathBuf>>,
    cwd: &Path,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
    file_commands: &[crate::discovery::FileCommand],
    approval_shared: std::sync::Arc<std::sync::Mutex<crate::approval::ApprovalMode>>,
    report_pins: &crate::session_stats::ReportPins,
) -> anyhow::Result<()> {
    let mut rl = DefaultEditor::new()?;
    let mut clock = crate::session_stats::SessionClock::new();
    println!("pirs — pi agent harness, Rust port. /help for commands, Ctrl-D to quit.");
    loop {
        match rl.readline("pirs> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                if line.starts_with('/') {
                    match handle_command(
                        line,
                        agent,
                        &session_path.clone(),
                        host,
                        file_commands,
                        printer,
                        &approval_shared,
                        &mut clock,
                        report_pins,
                    )
                    .await
                    {
                        Ok(true) => break,
                        Ok(false) => continue,
                        Err(e) => {
                            eprintln!("[command error: {e}]");
                            continue;
                        }
                    }
                }
                if let Some(cmd) = line.strip_prefix("!!") {
                    run_local_bash(cmd, cwd, false, agent).await;
                    continue;
                }
                if let Some(cmd) = line.strip_prefix('!') {
                    run_local_bash(cmd, cwd, true, agent).await;
                    continue;
                }
                let mode = *approval_shared.lock().unwrap();
                let sp = session_path.lock().unwrap().clone();
                clock.mark_user_turn();
                clock.agent_start();
                // Snapshot before the turn so /undo can rewind conversation.
                pirs_tools::rewind_snapshot(
                    &line.chars().take(80).collect::<String>(),
                    &agent.messages,
                );
                let before = agent.messages.len();
                let user_line = line.to_string();
                // false: the interactive REPL reads the next line with rustyline,
                // so a background stdin steer thread would race it and drop chars.
                if let Err(e) = run_turn(agent, line, printer, &sp, mode, host, false).await {
                    eprintln!("[error: {e}]");
                }
                clock.agent_end();
                clock.absorb_messages(&agent.messages[before..]);
                // Long-term memory of the user (soul + memory.db) when durable.
                if pirs_skills::learn_enabled_interactive()
                    || pirs_skills::looks_durable(&user_line)
                {
                    let reply = agent
                        .messages
                        .iter()
                        .rev()
                        .find_map(|m| match m {
                            pirs_ai::Message::Assistant(a) => {
                                let t = a.text();
                                if t.trim().is_empty() {
                                    None
                                } else {
                                    Some(t)
                                }
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let state_dir = cwd.join(".pirs");
                    let key = session_path
                        .lock()
                        .ok()
                        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| "repl".into());
                    pirs_skills::maybe_memory_nudge(
                        agent.provider.clone(),
                        &agent.model,
                        None, // env/auth store resolves keys
                        &state_dir,
                        &key,
                        &user_line,
                        &reply,
                    )
                    .await;
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => bail!(e),
        }
    }
    crate::session_stats::print_session_stats_pins(
        &clock,
        &agent.usage_report(),
        &agent.model,
        report_pins,
    );
    Ok(())
}

pub async fn run_local_bash(cmd: &str, cwd: &Path, record: bool, agent: &mut Agent) {
    let tool = pirs_tools::BashTool::new(cwd.to_path_buf());
    let out = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: format!("local-{}", pirs_ai::now_millis()),
            args: serde_json::json!({"command": cmd}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await;
    let text = match &out {
        Ok(o) => o
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => e.to_string(),
    };
    println!("{text}");
    if record {
        agent.messages.push(Message::user(format!(
            "User ran a local command: `{cmd}`\nOutput:\n{text}"
        )));
    }
}

pub async fn handle_command(
    line: &str,
    agent: &mut Agent,
    session_path: &std::sync::Arc<std::sync::Mutex<PathBuf>>,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
    file_commands: &[crate::discovery::FileCommand],
    printer: &Arc<Printer>,
    approval_shared: &std::sync::Arc<std::sync::Mutex<crate::approval::ApprovalMode>>,
    clock: &mut crate::session_stats::SessionClock,
    report_pins: &crate::session_stats::ReportPins,
) -> anyhow::Result<bool> {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "/quit" | "/exit" => return Ok(true),
        "/help" => {
            for fc in file_commands {
                println!(
                    "/{:<12} {}  [{}]",
                    fc.name,
                    fc.description,
                    fc.path.display()
                );
            }
            println!(
                "/model [id]     show or set model\n\
                 /stats          session wall time, agent time, tokens\n\
                 /usage          same as /stats\n\
                 /export <p>     export session to a JSONL file\n\
                 /compact        compact history now\n\
                 /undo           rewind conversation to previous snapshot\n\
                 /doctor         runtime diagnostics (keys, lsp, mcp, browser)\n\
                 /audit [n]      tail last N audit log lines\n\
                 /profile [p]    show or set agent safety profile\n\
                 /image <path>   attach image to next prompt (vision)\n\
                 /plan | /act    product dial (read-only vs full tools)\n\
                 /status         runtime features, autonomy, packs, caps\n\
                 /features       alias for /status\n\
                 /autonomy [m]   plan|edit|full  (one tool-access knob)\n\
                 /plan | /act    shortcuts for autonomy plan / full\n\
                 /permission [m] legacy alias for autonomy\n\
                 /checkpoint     list|create|restore [id]\n\
                 /approval       auto|ask|yolo (prompts; yolo→full autonomy)\n\
                 /fork [n]       fork session at entry\n\
                 /tree           session lineage\n\
                 /quit           exit (prints session stats)\n\
                 !<cmd>          run command locally, record output in context\n\
                 !!<cmd>         run command locally, do not record"
            );
        }
        "/undo" => match pirs_tools::host_undo(&mut agent.messages) {
            Ok(msg) => println!("{msg}"),
            Err(e) => eprintln!("[undo] {e}"),
        },
        "/doctor" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            for line in pirs_tools::doctor_report(&cwd) {
                println!("{line}");
            }
        }
        "/status" | "/features" | "/runtime" => {
            if let Some(mut snap) = crate::runtime_features::live() {
                snap.refresh_live_dials();
                println!("{}", snap.format_human());
            } else {
                println!("(runtime snapshot not ready)");
            }
        }
        "/audit" => {
            let n: usize = arg.parse().unwrap_or(40).clamp(1, 200);
            let path = pirs_agent::default_audit_path();
            if !path.is_file() {
                println!("no audit log yet at {}", path.display());
            } else {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(n);
                println!(
                    "audit {} (last {} of {}):\n{}",
                    path.display(),
                    lines.len() - start,
                    lines.len(),
                    lines[start..].join("\n")
                );
            }
        }
        "/profile" => {
            if arg.is_empty() {
                println!(
                    "agent-profile: {}",
                    std::env::var("PIRS_AGENT_PROFILE").unwrap_or_else(|_| "default".into())
                );
            } else if pirs_tools::SafetyProfile::parse(arg).is_some() {
                std::env::set_var("PIRS_AGENT_PROFILE", arg);
                println!("agent-profile set to {arg} (new denials apply on next tool call)");
            } else {
                println!("usage: /profile <default|plan|accept-edits|auto-approve>");
            }
        }
        "/image" => {
            if arg.is_empty() {
                println!("usage: /image <path-to-png-or-jpg>");
            } else {
                match attach_image_message(agent, Path::new(arg)) {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => eprintln!("[image] {e}"),
                }
            }
        }
        "/plan" | "/act" => {
            let mode = if cmd == "/plan" {
                pirs_tools::PermissionMode::ReadOnly
            } else {
                pirs_tools::PermissionMode::DangerFullAccess
            };
            pirs_tools::set_live_permission_mode(mode);
            if cmd == "/plan" {
                std::env::set_var("PIRS_AGENT_PROFILE", "plan");
            }
            println!(
                "mode → {} (permission={}; denials apply on next tool call)",
                cmd.trim_start_matches('/'),
                mode.name()
            );
        }
        "/checkpoint" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let action = if arg.is_empty() { "list" } else { arg };
            match action {
                "list" => {
                    for m in pirs_tools::list_checkpoints(&cwd) {
                        println!("{} {} {:?}", m.id, m.kind, m.label);
                    }
                }
                "create" => {
                    match pirs_tools::create_checkpoint(&cwd, "manual", agent.messages.len()) {
                        Ok(m) => println!("created {}", m.id),
                        Err(e) => eprintln!("[checkpoint] {e}"),
                    }
                }
                s if s.starts_with("restore") => {
                    let id = s.split_whitespace().nth(1);
                    match pirs_tools::restore_checkpoint(&cwd, id) {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => eprintln!("[checkpoint] {e}"),
                    }
                }
                _ => println!("usage: /checkpoint [list|create|restore [id]]"),
            }
        }
        "/permission" => {
            if arg.is_empty() {
                println!(
                    "permission-mode: {}",
                    pirs_tools::live_permission_mode().name()
                );
            } else if let Some(m) = pirs_tools::PermissionMode::parse(arg) {
                pirs_tools::set_live_permission_mode(m);
                println!("permission-mode → {}", m.name());
            } else {
                println!("usage: /permission read-only|workspace-write|danger-full-access");
            }
        }
        "/model" => {
            if arg.is_empty() {
                println!("current model: {}", agent.model);
            } else {
                agent.model = arg.to_string();
                println!("model set to {arg}");
            }
        }
        "/usage" | "/stats" => {
            crate::session_stats::print_session_stats_pins(
                clock,
                &agent.usage_report(),
                &agent.model,
                report_pins,
            );
        }
        "/approval" | "/autonomy" => {
            if arg.is_empty() {
                let a = match pirs_tools::live_permission_mode() {
                    pirs_tools::PermissionMode::ReadOnly => pirs_tools::Autonomy::Plan,
                    pirs_tools::PermissionMode::WorkspaceWrite => pirs_tools::Autonomy::Edit,
                    pirs_tools::PermissionMode::DangerFullAccess => pirs_tools::Autonomy::Full,
                };
                println!(
                    "{}  ·  approval prompts: {}",
                    pirs_tools::autonomy_status_line(a),
                    approval_shared.lock().unwrap().name()
                );
            } else if let Some(a) = pirs_tools::Autonomy::parse(arg) {
                pirs_tools::apply_autonomy(a);
                if a.is_yolo() {
                    *approval_shared.lock().unwrap() = crate::approval::ApprovalMode::Yolo;
                }
                println!("{}", pirs_tools::autonomy_status_line(a));
            } else if let Some(m) = crate::approval::ApprovalMode::parse(arg) {
                // Legacy: /approval yolo → full autonomy
                *approval_shared.lock().unwrap() = m;
                if m == crate::approval::ApprovalMode::Yolo {
                    pirs_tools::apply_autonomy(pirs_tools::Autonomy::Full);
                    println!(
                        "{}",
                        pirs_tools::autonomy_status_line(pirs_tools::Autonomy::Full)
                    );
                } else {
                    println!(
                        "approval prompts → {}  ({})",
                        m.name(),
                        pirs_tools::autonomy_status_line({
                            match pirs_tools::live_permission_mode() {
                                pirs_tools::PermissionMode::ReadOnly => pirs_tools::Autonomy::Plan,
                                pirs_tools::PermissionMode::WorkspaceWrite => {
                                    pirs_tools::Autonomy::Edit
                                }
                                pirs_tools::PermissionMode::DangerFullAccess => {
                                    pirs_tools::Autonomy::Full
                                }
                            }
                        })
                    );
                }
            } else {
                println!("usage: /autonomy plan|edit|full   or   /approval auto|ask|yolo");
            }
        }
        "/compact" => {
            println!("compacting...");
            let done = agent.compact_now().await;
            if done {
                println!("compacted ({} messages now)", agent.messages.len());
            } else {
                println!("nothing to compact (or compaction disabled)");
            }
        }
        "/fork" => {
            let idx: Option<usize> = if arg.is_empty() {
                None
            } else {
                Some(arg.parse()?)
            };
            let (new_path, messages, meta) =
                crate::session::fork_session(&session_path.lock().unwrap().clone(), idx)?;
            agent.messages = messages;
            println!(
                "forked at entry {} -> {} (parent: {})",
                meta.parent_entry.unwrap_or(0),
                new_path.display(),
                meta.parent_session.unwrap_or_default()
            );
            *session_path.lock().unwrap() = new_path;
        }
        "/tree" => {
            for (id, parent, parent_entry, entries) in
                crate::session::lineage(&session_path.lock().unwrap().clone())
            {
                println!(
                    "{id} ({} entries){}",
                    entries,
                    parent
                        .map(|p| format!(" <- fork of {p} @ {parent_entry:?}"))
                        .unwrap_or_default()
                );
            }
        }
        "/export" => {
            if arg.is_empty() {
                bail!("usage: /export <path>");
            }
            let dest = PathBuf::from(arg);
            std::fs::copy(session_path.lock().unwrap().clone(), &dest)
                .with_context(|| format!("failed to export to {}", dest.display()))?;
            println!("exported to {}", dest.display());
        }
        other => {
            let cmd_name = other.trim_start_matches('/');
            let mut handled = false;
            if let Some(h) = host {
                if h.commands().iter().any(|(n, _)| n == cmd_name) {
                    match h.run_command(cmd_name, arg) {
                        Ok(out) if !out.is_empty() => println!("{out}"),
                        Ok(_) => {}
                        Err(e) => eprintln!("[command error: {e}]"),
                    }
                    handled = true;
                }
            }
            if !handled {
                let cmd_name = other.trim_start_matches('/');
                if let Some(fc) = file_commands.iter().find(|c| c.name == cmd_name) {
                    let prompt = crate::discovery::expand_command(fc, arg);
                    let mode = *approval_shared.lock().unwrap();
                    let sp = session_path.lock().unwrap().clone();
                    // false: handle_command runs inside the interactive REPL loop,
                    // so a rustyline readline follows -- same stdin race as above.
                    if let Err(e) = run_turn(agent, &prompt, printer, &sp, mode, host, false).await
                    {
                        eprintln!("[error: {e}]");
                    }
                } else {
                    println!("unknown command: {other}");
                }
            }
        }
    }
    Ok(false)
}

/// Attach a local image as a multimodal user message (for vision models).
pub fn attach_image_message(agent: &mut Agent, path: &Path) -> anyhow::Result<String> {
    use base64::Engine as _;
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if !abs.is_file() {
        bail!("image not found: {}", abs.display());
    }
    let bytes = std::fs::read(&abs)?;
    if bytes.len() > 12 * 1024 * 1024 {
        bail!("image too large ({} bytes)", bytes.len());
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
        other => bail!("unsupported image type .{other}; use png/jpg/webp/gif"),
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
    Ok(format!(
        "attached {} ({} bytes) — send a follow-up message to discuss it",
        abs.display(),
        bytes.len()
    ))
}
