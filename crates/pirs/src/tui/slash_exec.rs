use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pirs_agent::Agent;
use pirs_ai::Message;

use crate::session_stats;

use super::app::{App, SessionControls};
use super::chat::ChatItem;
use super::model_picker::{ModelPicker, ModelPickerTarget};
use super::terminal::{copy_to_clipboard, last_assistant_text};

pub(super) fn handle_slash_command(
    app: &mut App,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
    text: &str,
    host: Option<&Arc<pirs_rhai::ExtensionHost>>,
) {
    let (cmd, arg) = match text.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (text, ""),
    };
    match cmd {
        "/help" | "/?" => {
            app.show_help = true;
            app.notice(
                "slash: /tour /model /plan-model /strategy /goal /stats /copy /undo /doctor \
                 /audit /profile /image /compact /plan /act /clear /quit  ·  type / + Tab \
                 ·  drag to select/copy (mouse off by default; PIRS_TUI_MOUSE=1 for wheel)",
            );
        }
        "/tour" | "/start" | "/onboard" => {
            app.push_tour_welcome();
            app.notice("tour restored — press 1–3 for starters, or type a goal");
        }
        "/model" => {
            if arg.is_empty() {
                open_model_picker(app, ModelPickerTarget::Exec, "");
            } else if arg == "?" || arg == "pick" || arg == "search" {
                open_model_picker(app, ModelPickerTarget::Exec, "");
            } else {
                // Direct set (pin or portable).
                match agent.try_lock() {
                    Ok(mut a) => {
                        a.model = arg.to_string();
                        app.model = arg.to_string();
                        let kind = if pirs_ai::ModelSpec::parse(arg).is_pin() {
                            "pin"
                        } else {
                            "portable"
                        };
                        app.notice(format!("model → {arg} ({kind}) · /model for fuzzy picker"));
                    }
                    Err(_) => {
                        app.notice("busy — wait for the current run to finish, then /model");
                    }
                }
            }
        }
        "/backends" => {
            crate::registry::load_secrets_env();
            let reg = crate::registry::load_registry_layers(&app.cwd);
            let mut lines = String::from("backends:\n");
            for b in &reg.backends {
                let has = pirs_ai::backend_key_present(b);
                let env = b.api_key_env.as_deref().unwrap_or("-");
                lines.push_str(&format!(
                    "  {:<18} key={}  env={env}\n",
                    b.name,
                    if has { "yes" } else { "no" }
                ));
            }
            lines.push_str(
                "add: /backend add <name> <url> <KEY_ENV>\nkey: /key KEY_ENV=sk-…\nmodel: /model",
            );
            app.push(ChatItem::System(lines));
            app.notice("backends listed (see chat)");
        }
        "/backend" => {
            // /backend add name url env [kind]
            let parts: Vec<&str> = arg.split_whitespace().collect();
            if parts.first().copied() != Some("add") || parts.len() < 4 {
                app.notice(
                    "usage: /backend add <name> <base_url> <API_KEY_ENV> [kind]\n  \
                     e.g. /backend add openrouter-work https://openrouter.ai/api/v1 OPENROUTER_WORK_API_KEY",
                );
                return;
            }
            let name = parts[1];
            let url = parts[2];
            let env = parts[3];
            let kind = parts.get(4).copied().unwrap_or("openai_compatible");
            match crate::secrets_edit::append_backend(name, url, env, kind) {
                Ok(path) => {
                    app.notice(format!(
                        "backend {name} → {} · set key: /key {env}=… · then /models refresh",
                        path.display()
                    ));
                }
                Err(e) => app.notice(format!("backend add: {e}")),
            }
        }
        "/key" => {
            let (name, value) = if let Some((n, v)) = arg.split_once('=') {
                (n.trim(), v.trim().to_string())
            } else {
                let mut sp = arg.split_whitespace();
                match (sp.next(), sp.collect::<Vec<_>>().join(" ")) {
                    (Some(n), v) if !v.is_empty() => (n, v),
                    _ => {
                        app.notice("usage: /key NAME=value  or  /key NAME value");
                        return;
                    }
                }
            };
            match crate::secrets_edit::set_secret_env(name, &value) {
                Ok(path) => {
                    // Mask value in chat.
                    let masked = if value.len() > 8 {
                        format!("{}…{}", &value[..4], &value[value.len() - 2..])
                    } else {
                        "***".into()
                    };
                    app.notice(format!(
                        "key {name}={masked} → {} (600) · live env updated",
                        path.display()
                    ));
                }
                Err(e) => app.notice(format!("key: {e}")),
            }
        }
        "/setup" => {
            crate::registry::load_secrets_env();
            let mut s = String::from("setup status\n");
            for line in crate::secrets_edit::setup_status_lines() {
                s.push_str(&line);
                s.push('\n');
            }
            s.push_str(
                "\n/key NAME=value          store in ~/.pirs/secrets.env\n\
                 /backend add n url ENV   append [[backends]]\n\
                 /models refresh          pull catalogs\n\
                 /model                   fuzzy pick model\n\
                 DashScope Coding Plan: User-Agent set automatically (PIRS_DASHSCOPE_UA)",
            );
            app.push(ChatItem::System(s));
            app.notice("setup status (see chat)");
        }
        "/thoughts" | "/think" | "/thinking" => {
            app.toggle_thoughts();
        }
        "/context" | "/roots" => {
            let ctx = pirs_tools::current_work_context();
            let mut s = format!("{}\n", ctx.summary_line());
            for r in &ctx.roots {
                s.push_str(&format!("  //{} → {}\n", r.name, r.path.display()));
            }
            if ctx.roots.len() > 1 {
                s.push_str(
                    "address: //name/rel/path  or  @name/rel  or  name:rel\n\
                     launch: pirs --cwd A --also B --also C\n\
                     or:     pirs --context NAME  (~/.pirs/contexts.toml)",
                );
            } else {
                s.push_str(
                    "single root. multi-repo: pirs --cwd A --also B\n\
                     or define [[context]] in ~/.pirs/contexts.toml",
                );
            }
            app.push(ChatItem::System(s));
            app.notice("work context (see chat)");
        }
        "/models" => {
            // /models [plan] [query…]  |  /models refresh
            let mut rest = arg.trim();
            if rest == "refresh" || rest.starts_with("refresh ") {
                let which = rest.strip_prefix("refresh").unwrap_or("").trim();
                app.notice("refreshing model catalogs…");
                let cwd = app.cwd.clone();
                let msg = tokio::task::block_in_place(|| {
                    crate::registry::load_secrets_env();
                    let reg = crate::registry::load_registry_layers(&cwd);
                    if which.is_empty() {
                        let results = pirs_ai::refresh_active(&reg);
                        if results.is_empty() {
                            return "no backends with keys — set OPENROUTER_API_KEY / DASHSCOPE_API_KEY in secrets.env".to_string();
                        }
                        let mut ok = 0usize;
                        let mut err = 0usize;
                        for (_n, r) in &results {
                            match r {
                                Ok(_) => ok += 1,
                                Err(_) => err += 1,
                            }
                        }
                        format!("catalogs: {ok} ok, {err} failed — /model to search")
                    } else {
                        match pirs_ai::refresh_backend(&reg, which) {
                            Ok((c, _)) => format!(
                                "refreshed {which}: {} models — /model to fuzzy search",
                                c.models.len()
                            ),
                            Err(e) => format!("refresh {which}: {e}"),
                        }
                    }
                });
                app.notice(msg);
                return;
            }
            let mut target = ModelPickerTarget::Exec;
            if let Some(r) = rest.strip_prefix("plan") {
                target = ModelPickerTarget::Plan;
                rest = r.trim();
            }
            open_model_picker(app, target, rest);
        }
        "/plan-model" => {
            if arg.is_empty() || arg == "?" || arg == "pick" || arg == "search" {
                open_model_picker(app, ModelPickerTarget::Plan, "");
            } else if arg == "none" || arg == "off" || arg == "clear" {
                app.plan_model = None;
                controls.lock().unwrap().plan_model = None;
                app.notice("plan-model cleared");
            } else {
                app.plan_model = Some(arg.to_string());
                controls.lock().unwrap().plan_model = Some(arg.to_string());
                let kind = if pirs_ai::ModelSpec::parse(arg).is_pin() {
                    "pin"
                } else {
                    "portable"
                };
                app.notice(format!(
                    "plan-model → {arg} ({kind}) · /plan-model for picker"
                ));
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
                match pirs_rhai::discover::resolve_strategy(
                    arg,
                    &std::env::current_dir().unwrap_or_default(),
                ) {
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
        "/usage" | "/stats" => match agent.try_lock() {
            Ok(a) => {
                let r = a.usage_report();
                let pins = app.report_pins();
                let text =
                    session_stats::format_session_stats_pins(&app.clock, &r, &app.model, &pins);
                app.notice(text);
            }
            Err(_) => app.notice("busy — try /stats after the run finishes"),
        },
        "/status" | "/features" | "/runtime" => {
            if let Some(mut snap) = crate::runtime_features::live() {
                snap.refresh_live_dials();
                // Keep model in sync with live app state.
                snap.model = app.model.clone();
                snap.plan_model = app.plan_model.clone();
                snap.strategy = app.strategy.clone();
                crate::runtime_features::publish(snap.clone());
                app.push(ChatItem::System(snap.format_human()));
                app.notice("runtime status (see chat)");
            } else {
                app.notice("runtime snapshot not ready");
            }
        }
        "/copy" | "/yank" => {
            // Prefer explicit arg, else last assistant bubble.
            let text = if !arg.is_empty() {
                Some(arg.to_string())
            } else {
                last_assistant_text(&app.items)
            };
            match text {
                None => app.notice("nothing to copy — no assistant reply yet"),
                Some(body) => match copy_to_clipboard(&body) {
                    Ok(()) => {
                        let n = body.chars().count();
                        app.notice(format!("copied {n} chars to clipboard"));
                    }
                    Err(e) => app.notice(format!("copy failed: {e}")),
                },
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
            pirs_tools::apply_autonomy(pirs_tools::Autonomy::Plan);
            app.approval_mode = "auto".into();
            if let Some(mut s) = crate::runtime_features::live() {
                s.refresh_live_dials();
                crate::runtime_features::publish(s);
            }
            app.notice(pirs_tools::autonomy_status_line(pirs_tools::Autonomy::Plan));
        }
        "/act" | "/edit" => {
            // /act = full tools (historical); /edit = workspace writes only.
            let a = if cmd == "/edit" {
                pirs_tools::Autonomy::Edit
            } else {
                pirs_tools::Autonomy::Full
            };
            pirs_tools::apply_autonomy(a);
            if a.is_yolo() {
                app.approval_mode = "yolo".into();
            }
            if let Some(mut s) = crate::runtime_features::live() {
                s.refresh_live_dials();
                crate::runtime_features::publish(s);
            }
            app.notice(pirs_tools::autonomy_status_line(a));
        }
        "/yolo" | "/full" => {
            pirs_tools::apply_autonomy(pirs_tools::Autonomy::Full);
            app.approval_mode = "yolo".into();
            if let Some(mut s) = crate::runtime_features::live() {
                s.refresh_live_dials();
                crate::runtime_features::publish(s);
            }
            app.notice(pirs_tools::autonomy_status_line(pirs_tools::Autonomy::Full));
        }
        "/autonomy" => {
            if arg.is_empty() {
                let a = match pirs_tools::live_permission_mode() {
                    pirs_tools::PermissionMode::ReadOnly => pirs_tools::Autonomy::Plan,
                    pirs_tools::PermissionMode::WorkspaceWrite => pirs_tools::Autonomy::Edit,
                    pirs_tools::PermissionMode::DangerFullAccess => pirs_tools::Autonomy::Full,
                };
                app.notice(pirs_tools::autonomy_status_line(a));
            } else if let Some(a) = pirs_tools::Autonomy::parse(arg) {
                pirs_tools::apply_autonomy(a);
                if a.is_yolo() {
                    app.approval_mode = "yolo".into();
                }
                if let Some(mut s) = crate::runtime_features::live() {
                    s.refresh_live_dials();
                    crate::runtime_features::publish(s);
                }
                app.notice(pirs_tools::autonomy_status_line(a));
            } else {
                app.notice("usage: /autonomy plan|edit|full");
            }
        }
        "/permission" => {
            // Legacy alias → autonomy ladder
            if arg.is_empty() {
                let a = match pirs_tools::live_permission_mode() {
                    pirs_tools::PermissionMode::ReadOnly => pirs_tools::Autonomy::Plan,
                    pirs_tools::PermissionMode::WorkspaceWrite => pirs_tools::Autonomy::Edit,
                    pirs_tools::PermissionMode::DangerFullAccess => pirs_tools::Autonomy::Full,
                };
                app.notice(pirs_tools::autonomy_status_line(a));
            } else if let Some(a) = pirs_tools::Autonomy::parse(arg).or_else(|| {
                pirs_tools::PermissionMode::parse(arg).map(|m| match m {
                    pirs_tools::PermissionMode::ReadOnly => pirs_tools::Autonomy::Plan,
                    pirs_tools::PermissionMode::WorkspaceWrite => pirs_tools::Autonomy::Edit,
                    pirs_tools::PermissionMode::DangerFullAccess => pirs_tools::Autonomy::Full,
                })
            }) {
                pirs_tools::apply_autonomy(a);
                if a.is_yolo() {
                    app.approval_mode = "yolo".into();
                }
                app.notice(pirs_tools::autonomy_status_line(a));
            } else {
                app.notice("usage: /autonomy plan|edit|full  (or legacy permission names)");
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
                    Ok(a) => match pirs_tools::create_checkpoint(&app.cwd, "tui", a.messages.len())
                    {
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
            // Extension slash commands (e.g. /goal from goal.rhai).
            let cmd_name = other.trim_start_matches('/');
            if let Some(h) = host {
                if h.commands().iter().any(|(n, _)| n == cmd_name) {
                    match h.run_command(cmd_name, arg) {
                        Ok(out) if !out.is_empty() => {
                            app.push(ChatItem::System(out));
                            app.notice(format!("/{cmd_name}"));
                        }
                        Ok(_) => app.notice(format!("/{cmd_name} done")),
                        Err(e) => app.notice(format!("/{cmd_name}: {e}")),
                    }
                    return;
                }
            }
            app.notice(format!("unknown command {other} — /help for slash list"));
        }
    }
}

pub(super) fn attach_image_to_agent(
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
    Ok(format!(
        "attached {} ({} bytes)",
        abs.display(),
        bytes.len()
    ))
}

// ── Model picker key handling (shared with input) ─────────────
pub(super) fn handle_model_picker_key(
    app: &mut App,
    key: KeyEvent,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
) -> bool {
    let Some(picker) = app.model_picker.as_mut() else {
        return false;
    };
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            app.model_picker = None;
            app.dirty = true;
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => return true,
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            if picker.sel > 0 {
                picker.sel -= 1;
            }
            app.dirty = true;
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
            if picker.sel + 1 < picker.hits.len() {
                picker.sel += 1;
            }
            app.dirty = true;
        }
        (KeyCode::PageUp, _) => {
            picker.sel = picker.sel.saturating_sub(8);
            app.dirty = true;
        }
        (KeyCode::PageDown, _) => {
            picker.sel = (picker.sel + 8).min(picker.hits.len().saturating_sub(1));
            app.dirty = true;
        }
        (KeyCode::Enter, _) => {
            if let Some(hit) = picker.selected().cloned() {
                let target = picker.target;
                app.model_picker = None;
                apply_model_choice(app, agent, controls, target, &hit.id);
            }
        }
        (KeyCode::Backspace, _) => {
            picker.query.pop();
            picker.refilter();
            app.dirty = true;
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            picker.query.clear();
            picker.refilter();
            app.dirty = true;
        }
        (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
            if !c.is_control() {
                picker.query.push(c);
                picker.refilter();
                app.dirty = true;
            }
        }
        _ => {}
    }
    false
}

pub(super) fn apply_model_choice(
    app: &mut App,
    agent: &Arc<tokio::sync::Mutex<Agent>>,
    controls: &Arc<Mutex<SessionControls>>,
    target: ModelPickerTarget,
    id: &str,
) {
    match target {
        ModelPickerTarget::Exec => match agent.try_lock() {
            Ok(mut a) => {
                a.model = id.to_string();
                app.model = id.to_string();
                let kind = if pirs_ai::ModelSpec::parse(id).is_pin() {
                    "pin"
                } else {
                    "portable"
                };
                app.notice(format!("model → {id} ({kind})"));
            }
            Err(_) => app.notice("busy — wait for the run, then pick a model again"),
        },
        ModelPickerTarget::Plan => {
            app.plan_model = Some(id.to_string());
            controls.lock().unwrap().plan_model = Some(id.to_string());
            let kind = if pirs_ai::ModelSpec::parse(id).is_pin() {
                "pin"
            } else {
                "portable"
            };
            app.notice(format!("plan-model → {id} ({kind})"));
        }
    }
}

pub(super) fn open_model_picker(app: &mut App, target: ModelPickerTarget, query: &str) {
    app.show_help = false;
    // Prefer App.model_aliases (seeded from CLI registry) so they are not dead data.
    app.model_picker = Some(ModelPicker::open_with_aliases(
        target,
        query,
        &app.model_aliases,
    ));
    app.dirty = true;
    let n = app
        .model_picker
        .as_ref()
        .map(|p| p.universe.len())
        .unwrap_or(0);
    app.set_status(format!(
        "model picker · {n} candidates · type to fuzzy filter"
    ));
}
