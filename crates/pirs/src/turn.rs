//! One agent turn (simple + strategy) and stdin steer handle.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use anyhow::Context as _;
use std::sync::{Arc, Mutex};
use pirs_agent::{Agent, AgentTool};
use pirs_ai::Message;
use pirs_rhai::ExtensionHost;

use crate::printer::Printer;

pub async fn run_turn(
    agent: &mut Agent,
    input: &str,
    _printer: &Arc<Printer>,
    _session_path: &Path,
    approval_mode: crate::approval::ApprovalMode,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
    steer_from_stdin: bool,
) -> anyhow::Result<()> {
    let cancel = agent.cancel_handle();
    // The stdin steer thread lets you inject a line into the *running* turn, but
    // it reads stdin in the background and its stop() only signals a flag the
    // thread checks *before* its blocking read -- so once a turn ends it stays
    // parked in that read, competing with the next rustyline `readline` for the
    // same terminal fd and stealing keystrokes (dropped characters). The
    // interactive REPL therefore opts out (`steer_from_stdin = false`): with no
    // background reader, whatever you type during a turn stays in the terminal's
    // line buffer and rustyline picks it up as your next line (type-ahead). Only
    // callers with no subsequent readline (one-shot) keep it on.
    let steer_handle = if approval_mode == crate::approval::ApprovalMode::Ask || !steer_from_stdin {
        None
    } else {
        Some(SteerHandle::start(agent))
    };

    let mut run = std::pin::pin!(agent.prompt(input));
    let result = loop {
        tokio::select! {
            r = &mut run => break r,
            _ = tokio::signal::ctrl_c() => {
                cancel.lock().unwrap().cancel();
            }
        }
    };
    if let Some(h) = steer_handle {
        h.stop();
    }

    result?;
    if let Some(h) = host {
        for err in h.drain_hook_errors() {
            eprintln!("[extension error] {err}");
        }
    }
    Ok(())
}

/// Tools a read-only (planning/critique) phase may use: navigation and search
/// only, nothing that can change the tree. An allowlist — not a denylist — so a
/// newly added mutating tool can never silently leak into a planner's scope.
const READONLY_PHASE_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "recall",
    "code_map",
    "lsp",
    "doctor",
    "audit_tail",
    "research",
    "web_fetch",
    "web_search",
    "fleet",
    "pr",
];

/// Run a shell verification command in `cwd`. Returns `(passed, output_tail)`;
/// the last 4000 chars of combined stdout+stderr (errors cluster at the end) are
/// what feeds the next attempt's verdict.
pub async fn run_verify_command(cmd: String, cwd: PathBuf) -> (bool, String) {
    let result = tokio::task::spawn_blocking(move || {
        let ev = pirs_agent::GreenEvidence::from_command(&cmd, &cwd);
        (ev.passed, format!("{}\n{}", ev.summary_line(), ev.output_tail))
    })
    .await;
    match result {
        Ok(pair) => pair,
        Err(e) => (false, format!("verify task panicked: {e}")),
    }
}

/// Run a one-shot prompt through a loop strategy/profile on the real agent, with
/// an optional verify-and-retry gate.
///
/// Each phase forks the fully wired `base` agent (same hooks, listeners, session
/// persistence, completion), re-scoped to the phase's tools and model. When
/// `verify` is set, the whole strategy re-runs (up to `max_attempts`, default 3)
/// with the failing command's output fed back as the next attempt's verdict.
/// Returns a usage report spanning every phase of every attempt.
#[allow(clippy::too_many_arguments)]
pub async fn run_strategy_turn(
    base: &Agent,
    input: &str,
    strategy_arg: Option<&str>,
    profile_arg: Option<&str>,
    default_model: &str,
    plan_model: Option<&str>,
    full_tools: Vec<Arc<dyn AgentTool>>,
    cwd: &Path,
    verify: Option<&str>,
    max_attempts: Option<u32>,
    recorder: Option<&Arc<pirs_agent::trace::Recorder>>,
    trace_phase: Option<Arc<Mutex<String>>>,
) -> anyhow::Result<(pirs_agent::usage::UsageReport, bool)> {
use pirs_agent::gate::{run_gated, GateOutcome};
use pirs_agent::phase_agent::AgentPhaseDriver;
use pirs_agent::profile::Profile;
use pirs_agent::strategy::{pin_plan_model, run_strategy_async, PhaseReq, Task, ToolScope};
    use std::cell::RefCell;
    use std::rc::Rc;

    // Effective profile: a neutral wrapper when only --strategy is given. A
    // --strategy always overrides which strategy the profile runs, keeping the
    // profile's persona, model, and tool policy.
    let mut profile = match profile_arg {
        Some(p) => pirs_rhai::discover::resolve_profile(p, cwd)
            .with_context(|| format!("resolving profile {p:?}"))?,
        // Placeholder strategy; always replaced below because reaching here means
        // --strategy was given (strategy_mode with no --profile).
        None => Profile::from_strategy(
            "adhoc",
            pirs_rhai::builtins::builtin("monolithic").expect("monolithic is a built-in"),
        ),
    };
    if let Some(s) = strategy_arg {
        profile.strategy = pirs_rhai::discover::resolve_strategy(s, cwd)
            .with_context(|| format!("resolving strategy {s:?}"))?;
    }
    let mut strategy = profile.resolved_strategy();
    // Strong plan / weak exec: pin read-only phases to --plan-model; full-scope
    // executor keeps profile/script model or falls back to --model (default_model).
    if let Some(pm) = plan_model {
        pin_plan_model(&mut strategy, pm);
    }
    let policy = profile.tools.clone();

    // Retry only makes sense with a gate; default to 3 attempts when verifying.
    let attempts = max_attempts.unwrap_or(if verify.is_some() { 3 } else { 1 });

    eprintln!(
        "[strategy '{}' · {} step(s){}{}{}]",
        strategy.name,
        strategy.steps.len(),
        profile_arg
            .map(|p| format!(" · profile '{p}'"))
            .unwrap_or_default(),
        plan_model
            .map(|m| format!(" · plan-model '{m}' · exec-model '{default_model}'"))
            .unwrap_or_default(),
        verify
            .map(|_| format!(" · verify (≤{attempts} attempts)"))
            .unwrap_or_default(),
    );

    // All phases of all attempts accumulate here for one run-wide usage report.
    let all_messages: Rc<RefCell<Vec<Message>>> = Rc::new(RefCell::new(Vec::new()));
    let default_model = default_model.to_string();
    let strategy_ref = &strategy;
    let policy_ref = &policy;
    let tools_ref = &full_tools;
    let model_ref = default_model.as_str();
    let rec_owned = recorder.cloned();
    let phase_slot = trace_phase.unwrap_or_else(|| Arc::new(Mutex::new("main".into())));

    // One strategy attempt: a fresh driver seeded with the prior failure verdict.
    let attempt = |verdict: Option<String>| {
        let all_messages = Rc::clone(&all_messages);
        let rec = rec_owned.clone();
        let phase_slot = Arc::clone(&phase_slot);
        async move {
            let mut driver = AgentPhaseDriver::new(|req: &PhaseReq| {
                // Profile tool policy first (a role can forbid tools entirely),
                // then the phase's read/write scope narrows a planner to nav-only.
                let mut scoped: Vec<Arc<dyn AgentTool>> = tools_ref
                    .iter()
                    .filter(|t| policy_ref.permits(t.name()))
                    .cloned()
                    .collect();
                if req.scope == ToolScope::ReadOnly {
                    scoped.retain(|t| READONLY_PHASE_TOOLS.contains(&t.name()));
                }
                let model = req.model.clone().unwrap_or_else(|| model_ref.to_string());
                eprintln!(
                    "\n\x1b[2m── phase {} · model {} · {}\x1b[0m",
                    req.phase_id,
                    model,
                    if req.scope == ToolScope::ReadOnly {
                        "read-only"
                    } else {
                        "full"
                    },
                );
                if let Ok(mut p) = phase_slot.lock() {
                    *p = req.phase_id.clone();
                }
                if let Some(rec) = &rec {
                    crate::observability::record_phase_start(rec, req);
                }
                // Per-phase model so telemetry packs / session_meta see the active one.
                pirs_rhai::set_session_meta(&pirs_rhai::current_session_id(), &model);
                base.fork_for_phase(req.system.clone(), model, scoped)
            });
            let task = Task {
                issue: input.to_string(),
                targets: Vec::new(),
                verdict,
            };
            let result = run_strategy_async(strategy_ref, &mut driver, &task).await;
            if let Some(rec) = &rec {
                // Pair phase.start with phase.end (last active phase id + transcript size).
                let phase_id = phase_slot
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_else(|_| "strategy".into());
                let output_chars: usize = driver
                    .messages()
                    .iter()
                    .map(|m| match m {
                        Message::Assistant(a) => a.text().len(),
                        Message::User(u) => match &u.content {
                            pirs_ai::UserContent::Text(t) => t.len(),
                            pirs_ai::UserContent::Blocks(bs) => bs
                                .iter()
                                .filter_map(|b| b.as_text())
                                .map(|t| t.len())
                                .sum(),
                        },
                        Message::ToolResult(r) => r
                            .content
                            .iter()
                            .filter_map(|b| b.as_text())
                            .map(|t| t.len())
                            .sum(),
                    })
                    .sum();
                crate::observability::record_phase_end(
                    rec,
                    &phase_id,
                    output_chars,
                    result.is_ok(),
                );
                rec.event(
                    "strategy.attempt_end",
                    serde_json::json!({ "ok": result.is_ok() }),
                );
            }
            all_messages
                .borrow_mut()
                .extend(driver.messages().iter().cloned());
            result
        }
    };

    // The gate: run the verify command (no command → always passes, single run).
    let verify_gate = || async move {
        let cmd = verify?;
        eprintln!("\n[verify: {cmd}]");
        let (ok, output) = run_verify_command(cmd.to_string(), cwd.to_path_buf()).await;
        if ok {
            eprintln!("[verify passed]");
            None
        } else {
            eprintln!("[verify failed — feeding the failure back to the next attempt]");
            Some(output)
        }
    };

    // Ctrl-C aborts the whole gated run by dropping its future, cancelling the
    // in-flight provider stream at its await point. The future is scoped to this
    // block so its borrows are released before we read the accumulated usage.
    let result: anyhow::Result<GateOutcome> = {
        let gated = run_gated(attempts, attempt, verify_gate);
        tokio::select! {
            r = gated => r,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[interrupted]");
                Err(anyhow::anyhow!("interrupted"))
            }
        }
    };

    let report = pirs_agent::usage::usage_report(&all_messages.borrow(), pirs_ai::Usage::default());
    let passed = match result? {
        GateOutcome::Passed { on_attempt } => {
            if verify.is_some() {
                eprintln!("\n[strategy passed the gate on attempt {on_attempt}]");
            }
            true
        }
        GateOutcome::Exhausted { .. } => {
            eprintln!("\n[strategy did not pass the gate after {attempts} attempt(s)]");
            false
        }
    };
    Ok((report, passed))
}

pub struct SteerHandle {
    tx: std::sync::mpsc::Sender<()>,
}

impl SteerHandle {
    fn start(agent: &Agent) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let steer = agent.steer_sender();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let mut lines = stdin.lock().lines();
            loop {
                if rx.try_recv().is_ok() {
                    break;
                }
                match lines.next() {
                    Some(Ok(line)) if !line.trim().is_empty() => {
                        steer(Message::user(line));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        });
        SteerHandle { tx }
    }

    fn stop(self) {
        let _ = self.tx.send(());
    }
}
