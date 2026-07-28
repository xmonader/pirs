use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _};
use pirs_agent::{AgentTool, Hooks, ToolExecContext, ToolOutput, ToolResultPatch};
use pirs_ai::{ContentBlock, ToolResultMessage};
use rhai::{Dynamic, Engine, Scope, AST};
use serde_json::Value;

pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    ext: usize,
}

pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    ext: usize,
}

pub struct Extension {
    pub path: PathBuf,
    engine: Engine,
    ast: AST,
    scope: Scope<'static>,
    pub caps: caps::Caps,
}

pub type SubagentRunner =
    Arc<dyn Fn(String, Option<String>) -> Result<String, String> + Send + Sync>;

pub mod builtins;
pub mod caps;
mod convert;
mod trust;
pub mod discover;
pub mod profile_script;
pub mod strategy_script;
pub mod weak_packs;

/// Immutable per-extension hook presence, hoisted OUT of `Mutex<Extension>` so
/// dispatchers can tell whether an extension even has a hook without taking its
/// lock. Without this, a busy extension (running its own tool) hard-blocks
/// every concurrent tool call — and drops `on_tool_result` patches — even when
/// it has no relevant hook at all. Indexed identically to `extensions`.
#[derive(Clone, Copy, Default)]
struct ExtFlags {
    has_on_tool_call: bool,
    has_on_tool_result: bool,
    has_on_context: bool,
    has_on_should_stop: bool,
    has_on_steering: bool,
    has_on_follow_up: bool,
    has_on_event: bool,
}

pub struct ExtensionHost {
    extensions: Vec<Mutex<Extension>>,
    ext_flags: Vec<ExtFlags>,
    tool_registry: Vec<RegisteredTool>,
    command_registry: Vec<RegisteredCommand>,
    subagent_runner: Mutex<Option<SubagentRunner>>,
    hook_errors: Mutex<Vec<String>>,
    inbox: Arc<Mutex<Vec<(String, String)>>>,
    pub load_errors: Vec<String>,
}

type StateStore = Arc<Mutex<std::collections::BTreeMap<String, Dynamic>>>;

fn cache_path_for(key: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut name = String::new();
    for c in key.chars() {
        name.push(if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            c
        } else {
            '_'
        });
    }
    std::path::Path::new(&home)
        .join(".pirs")
        .join("cache")
        .join(format!("{name}.json"))
}

/// Process-wide session identity exposed to scripts via `session_id()` and
/// `agent_model()`. Set once at startup; empty strings mean "unknown".
static SESSION_META: std::sync::RwLock<(String, String)> =
    std::sync::RwLock::new((String::new(), String::new()));

/// Set the session id and model name exposed to extension scripts.
pub fn set_session_meta(session_id: &str, model: &str) {
    *SESSION_META.write().unwrap() = (session_id.to_string(), model.to_string());
}

/// Current session id for host code (strategy phases, telemetry).
pub fn current_session_id() -> String {
    SESSION_META.read().unwrap().0.clone()
}

/// Current agent model string for host code.
pub fn current_agent_model() -> String {
    SESSION_META.read().unwrap().1.clone()
}

/// Query functions contributed by the embedding application (e.g. the CLI
/// exposes the code graph). Each becomes a rhai fn `name(path) -> [String]`.
/// Register before loading extensions.
type QueryFn = std::sync::Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;
static QUERY_FNS: std::sync::RwLock<Vec<(String, QueryFn)>> = std::sync::RwLock::new(Vec::new());

/// Register a host query fn available to all subsequently-loaded extensions.
/// Replaces any existing registration under the same name (idempotent).
pub fn register_query_fn(name: &str, f: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) {
    let mut g = QUERY_FNS.write().unwrap();
    g.retain(|(n, _)| n != name);
    g.push((name.to_string(), std::sync::Arc::new(f)));
}

/// Register standard host APIs for project toolchain + skills (shared core).
/// Call once before loading extensions so packs can call `project_profile(cwd)`.
/// Safe to call multiple times (replaces prior registrations).
pub fn register_core_host_apis() {
    register_query_fn("project_profile", |cwd| {
        let p = pirs_tools::detect_profile(std::path::Path::new(cwd));
        let mut lines = Vec::new();
        if let Some(t) = p.toolchain {
            lines.push(format!("toolchain={t}"));
        }
        for (k, v) in [
            ("test", p.test),
            ("lint", p.lint),
            ("typecheck", p.typecheck),
            ("build", p.build),
            ("format", p.format),
            ("run", p.run),
        ] {
            if let Some(cmd) = v {
                lines.push(format!("{k}={cmd}"));
            }
        }
        lines
    });
    register_query_fn("project_packages", |cwd| {
        pirs_tools::discover_packages(std::path::Path::new(cwd))
            .into_iter()
            .map(|p| format!("{}|{}|{}", p.name, p.path, p.toolchain.unwrap_or_default()))
            .collect()
    });
    register_query_fn("skills_index", |_arg| {
        pirs_skills::discover_skills(std::path::Path::new("."))
            .into_iter()
            .map(|s| format!("{}: {}", s.name, s.description))
            .collect()
    });
    // Active safety profile (PIRS_AGENT_PROFILE / --agent-profile). Packs may
    // *add* denials when profile is plan; they must never loosen Rust hard denials.
    register_query_fn("agent_profile", |_arg| {
        let name = std::env::var("PIRS_AGENT_PROFILE")
            .ok()
            .and_then(|s| pirs_tools::SafetyProfile::parse(&s).map(|p| p.name().to_string()))
            .unwrap_or_else(|| "default".into());
        vec![name]
    });
    // Core checkpoint create/restore (same path as tool `checkpoint` / `/checkpoint`).
    // Packs call `checkpoint_create(label)` / `checkpoint_restore(id)`.
    // Optional cwd: "label|/abs/cwd" or "id|/abs/cwd" so packs aren't cwd-racey.
    register_query_fn("checkpoint_create", |arg| {
        let (label, cwd) = split_label_cwd(arg);
        let label = if label.is_empty() { "auto".into() } else { label };
        match pirs_tools::create_checkpoint(&cwd, &label, 0) {
            Ok(m) => vec![m.id, m.kind, format!("label={}", m.label)],
            Err(e) => vec![format!("error:{e}")],
        }
    });
    register_query_fn("checkpoint_restore", |arg| {
        let (id, cwd) = split_label_cwd(arg);
        let id_opt = if id.is_empty() { None } else { Some(id.as_str()) };
        match pirs_tools::restore_checkpoint(&cwd, id_opt) {
            Ok(msg) => vec![msg],
            Err(e) => vec![format!("error:{e}")],
        }
    });
    register_query_fn("checkpoint_list", |arg| {
        let cwd = if arg.trim().is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(arg.trim())
        };
        pirs_tools::list_checkpoints(&cwd)
            .into_iter()
            .map(|m| format!("{}|{}|{}", m.id, m.kind, m.label))
            .collect()
    });
    // Deterministic review plan + reviewer context for review-gate and packs.
    // Arg: "" | "cwd" | "cwd|from|to". Returns [json_report, reviewer_context] or [error:…].
    register_query_fn("review_report", |arg| pirs_tools::host_review_report(arg));
}

#[cfg(test)]
mod review_host_tests {
    use super::*;

    #[test]
    fn review_report_host_fn_is_registered() {
        register_core_host_apis();
        let names: Vec<String> = QUERY_FNS
            .read()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            names.iter().any(|n| n == "review_report"),
            "review_report missing from host APIs: {names:?}"
        );
    }

    #[test]
    fn review_report_query_returns_json_on_temp_repo() {
        register_core_host_apis();
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        };
        run(&["init"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.rs"), "fn a(){}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "i"]);
        std::fs::write(dir.path().join("a.rs"), "fn a(){ x.unwrap(); }\n").unwrap();

        let f = QUERY_FNS
            .read()
            .unwrap()
            .iter()
            .find(|(n, _)| n == "review_report")
            .map(|(_, f)| f.clone())
            .expect("review_report registered");
        let lines = f(&dir.path().display().to_string());
        assert!(!lines.is_empty());
        assert!(
            !lines[0].starts_with("error:"),
            "unexpected error: {:?}",
            lines[0]
        );
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(v.get("tool_diet").is_some());
        assert_eq!(
            v["tool_diet"]["mutation_allowed"],
            serde_json::Value::Bool(false)
        );
        assert!(lines.len() >= 2);
        assert!(
            lines[1].contains("DENIED") || lines[1].contains("denied") || lines[1].contains("write"),
            "context missing diet: {}",
            lines[1]
        );
    }
}

/// Parse `label` or `label|/abs/cwd` for checkpoint host APIs.
///
/// Absolute cwd is only accepted when it stays under the process current
/// directory (or `PIRS_ALLOW_OUTSIDE_CWD=1`). Prevents pack-driven restore of
/// arbitrary host trees.
fn split_label_cwd(arg: &str) -> (String, std::path::PathBuf) {
    let arg = arg.trim();
    let process_cwd =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Some((label, cwd)) = arg.rsplit_once('|') {
        let cand = std::path::PathBuf::from(cwd.trim());
        if cand.is_absolute() || cwd.starts_with('.') {
            if let Ok(resolved) = pirs_tools::paths::resolve_contained(&process_cwd, cwd.trim()) {
                return (label.trim().to_string(), resolved);
            }
            // Outside process cwd — fall back to process cwd (safe default).
            eprintln!(
                "[rhai] checkpoint cwd {cwd:?} escapes process cwd; using {}",
                process_cwd.display()
            );
            return (label.trim().to_string(), process_cwd);
        }
    }
    (arg.to_string(), process_cwd)
}

fn build_engine(state: &StateStore, caps: &caps::Caps) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(200_000);
    engine.set_max_call_levels(32);
    engine.set_max_expr_depths(128, 128);
    engine.set_max_string_size(2 * 1024 * 1024);
    engine.set_max_array_size(100_000);
    engine.set_max_map_size(10_000);

    let get_state = Arc::clone(state);
    engine.register_fn("state_get", move |key: &str| -> Dynamic {
        get_state
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or(Dynamic::UNIT)
    });
    let set_state = Arc::clone(state);
    engine.register_fn("state_set", move |key: &str, value: Dynamic| {
        set_state.lock().unwrap().insert(key.to_string(), value);
    });
    let has_state = Arc::clone(state);
    engine.register_fn("state_has", move |key: &str| -> bool {
        has_state.lock().unwrap().contains_key(key)
    });
    let del_state = Arc::clone(state);
    engine.register_fn("state_del", move |key: &str| {
        del_state.lock().unwrap().remove(key);
    });
    engine.register_fn("str_join", |arr: rhai::Array, sep: &str| -> String {
        arr.iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(sep)
    });
    engine.register_fn("cache_get", |key: &str| -> Dynamic {
        let path = cache_path_for(key);
        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(v) => rhai::serde::to_dynamic(&v).unwrap_or(Dynamic::UNIT),
                Err(_) => Dynamic::UNIT,
            },
            Err(_) => Dynamic::UNIT,
        }
    });
    engine.register_fn("cache_put", |key: &str, value: Dynamic| -> bool {
        let path = cache_path_for(key);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json: serde_json::Value = match rhai::serde::from_dynamic(&value) {
            Ok(v) => v,
            Err(_) => return false,
        };
        std::fs::write(path, json.to_string()).is_ok()
    });
    engine.register_fn("sha256_hex", |data: &str| -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(data.as_bytes());
        format!("{:x}", h.finalize())
    });
    engine.register_fn("session_id", || -> String {
        SESSION_META.read().unwrap().0.clone()
    });
    engine.register_fn("agent_model", || -> String {
        SESSION_META.read().unwrap().1.clone()
    });
    for (name, f) in QUERY_FNS.read().unwrap().iter() {
        let f = std::sync::Arc::clone(f);
        engine.register_fn(name, move |path: &str| -> rhai::Array {
            f(path).into_iter().map(Dynamic::from).collect()
        });
    }
    engine.register_fn("now_millis", || -> rhai::INT {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as rhai::INT)
            .unwrap_or(0)
    });
    let caps_append = caps.clone();
    engine.register_fn("fs_append", move |path: &str, content: &str| -> bool {
        use std::io::Write;
        if !caps::check_fs(&caps_append, path) {
            return false;
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(content.as_bytes()))
            .is_ok()
    });
    let caps_read = caps.clone();
    engine.register_fn("fs_read", move |path: &str| -> String {
        if !caps::check_fs(&caps_read, path) {
            return String::new();
        }
        std::fs::read_to_string(path).unwrap_or_default()
    });
    let caps_write = caps.clone();
    engine.register_fn("fs_write", move |path: &str, content: &str| -> bool {
        if !caps::check_fs(&caps_write, path) {
            return false;
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        std::fs::write(path, content).is_ok()
    });
    let caps_exec = caps.clone();
    engine.register_fn("exec", move |command: &str| -> rhai::Map {
        exec_capped(&caps_exec, command, 30)
    });
    let caps_exec2 = caps.clone();
    engine.register_fn(
        "exec",
        move |command: &str, timeout_secs: rhai::INT| -> rhai::Map {
            exec_capped(&caps_exec2, command, timeout_secs.max(1) as u64)
        },
    );
    engine
}

/// exec gated by the capability manifest: a blocked command returns a
/// visible error map instead of running.
fn exec_capped(caps: &caps::Caps, command: &str, timeout_secs: u64) -> rhai::Map {
    if let Err(reason) = caps::check_exec(caps, command) {
        let mut map = rhai::Map::new();
        map.insert("output".into(), reason.into());
        map.insert("code".into(), (-1).into());
        map.insert("timedOut".into(), false.into());
        return map;
    }
    exec_impl(command, timeout_secs)
}

fn exec_impl(command: &str, timeout_secs: u64) -> rhai::Map {
    let mut map = rhai::Map::new();
    // Cap absurd timeouts (i64::MAX → Instant panic / multi-day wedges).
    let timeout_secs = timeout_secs.min(7 * 86400).max(1);
    let spawned = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map(|mut c| {
            #[cfg(unix)]
            {
                let _ = &mut c;
                unsafe {
                    let pid = c.id() as i32;
                    libc::setpgid(pid, pid);
                }
            }
            c
        });
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            map.insert("output".into(), format!("spawn failed: {e}").into());
            map.insert("code".into(), (-1).into());
            map.insert("timedOut".into(), false.into());
            return map;
        }
    };
    let pid = child.id();

    fn read_all_capped<R: std::io::Read + Send + 'static>(
        mut r: R,
        max_bytes: usize,
    ) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < max_bytes {
                            let take = n.min(max_bytes.saturating_sub(buf.len()));
                            buf.extend_from_slice(&chunk[..take]);
                        }
                        // Keep draining so the child can exit (avoid pipe deadlock).
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        rx
    }
    // Cap buffered output to avoid OOM; always drain pipes.
    let out_rx = read_all_capped(child.stdout.take().expect("piped"), 1_000_000);
    let err_rx = read_all_capped(child.stderr.take().expect("piped"), 512_000);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut status = None;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    timed_out = true;
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    // Never block forever on pipe EOF after kill (C-4): bounded recv.
    let recv_deadline = std::time::Duration::from_secs(2);
    let stdout = out_rx.recv_timeout(recv_deadline).unwrap_or_default();
    let stderr = err_rx.recv_timeout(recv_deadline).unwrap_or_default();
    let mut combined = stdout;
    combined.push_str(&stderr);
    if combined.chars().count() > 10_000 {
        combined = format!(
            "{}...[truncated]",
            combined.chars().take(10_000).collect::<String>()
        );
    }
    map.insert("output".into(), combined.into());
    map.insert(
        "code".into(),
        Dynamic::from(status.and_then(|s| s.code()).unwrap_or(-1) as i64),
    );
    map.insert("timedOut".into(), timed_out.into());
    map
}

impl ExtensionHost {
    pub fn new() -> Self {
        ExtensionHost {
            extensions: Vec::new(),
            ext_flags: Vec::new(),
            tool_registry: Vec::new(),
            command_registry: Vec::new(),
            subagent_runner: Mutex::new(None),
            hook_errors: Mutex::new(Vec::new()),
            inbox: Arc::new(Mutex::new(Vec::new())),
            load_errors: Vec::new(),
        }
    }

    pub fn inbox_drain(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }

    pub fn drain_hook_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.hook_errors.lock().unwrap())
    }

    fn record_error(&self, what: &str, e: impl std::fmt::Display) {
        let msg = format!("{what}: {e}");
        tracing::warn!("{msg}");
        let mut errors = self.hook_errors.lock().unwrap();
        if errors.len() < 100 {
            errors.push(msg);
        }
    }

    /// Wire the ability for scripts to spawn fresh-context sub-agents.
    /// Must be called before load_script for scripts that use run_subagent.
    pub fn set_subagent_runner(&mut self, runner: SubagentRunner) {
        *self.subagent_runner.lock().unwrap() = Some(runner);
    }

    pub fn has_subagent_runner(&self) -> bool {
        self.subagent_runner.lock().unwrap().is_some()
    }

    pub fn load_default_dirs(&mut self, cwd: &Path) {
        self.load_default_dirs_with_trust(cwd, &mut |dir| trust::prompt_trust(dir));
    }

    pub fn load_default_dirs_with_trust(
        &mut self,
        cwd: &Path,
        trust_decider: &mut dyn FnMut(&Path) -> TrustDecision,
    ) {
        let project_dir = cwd.join(".pirs").join("extensions");
        let mut dirs = Vec::new();
        match trust_decider(&project_dir) {
            TrustDecision::Allow => dirs.push(project_dir),
            TrustDecision::Deny => {
                self.load_errors.push(format!(
                    "{}: skipped (untrusted project extensions)",
                    project_dir.display()
                ));
            }
            TrustDecision::Skip => {}
        }
        if let Ok(home) = std::env::var("HOME") {
            let global = Path::new(&home).join(".pirs").join("extensions");
            if !dirs.contains(&global) {
                dirs.push(global);
            }
            // Packs installed via `pirs pack install` land here. Unlike the
            // hand-curated extensions dir, this holds remote code, so it is
            // trust-gated (hash-bound) exactly like a project dir — it never
            // auto-runs just because a file was written to it.
            let packs = Path::new(&home).join(".pirs").join("packs");
            if packs.exists() && !dirs.contains(&packs) {
                match trust_decider(&packs) {
                    TrustDecision::Allow => dirs.push(packs),
                    TrustDecision::Deny => self.load_errors.push(format!(
                        "{}: skipped (untrusted installed packs)",
                        packs.display()
                    )),
                    TrustDecision::Skip => {}
                }
            }
        }
        for dir in dirs {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut scripts: Vec<PathBuf> = read
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rhai"))
                .collect();
            scripts.sort();
            for script in scripts {
                if let Err(e) = self.load_script(&script) {
                    self.load_errors.push(format!("{}: {e}", script.display()));
                }
            }
        }
    }

    pub fn load_script(&mut self, path: &Path) -> anyhow::Result<()> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.load_source(&source, path.display().to_string())
    }

    pub fn load_source(&mut self, source: &str, name: String) -> anyhow::Result<()> {
        let ext_index = self.extensions.len();
        let caps = caps::parse_caps(source);
        let registered: Arc<Mutex<Vec<(String, String, rhai::Map)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let registered_cmds: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let state: StateStore = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
        let mut engine = build_engine(&state, &caps);

        let registrations = Arc::clone(&registered);
        engine.register_fn(
            "register_tool",
            move |name: &str, description: &str, schema: rhai::Map| {
                registrations.lock().unwrap().push((
                    name.to_string(),
                    description.to_string(),
                    schema,
                ));
            },
        );
        let cmd_registrations = Arc::clone(&registered_cmds);
        engine.register_fn("register_command", move |name: &str, description: &str| {
            cmd_registrations
                .lock()
                .unwrap()
                .push((name.to_string(), description.to_string()));
        });

        let runner_opt = self.subagent_runner.lock().unwrap().clone();
        if let Some(runner) = runner_opt.clone() {
            let sub_ok = caps::subagents_allowed(&caps);
            let r1 = Arc::clone(&runner);
            engine.register_fn("run_subagent", move |task: &str| -> String {
                if !sub_ok {
                    return "sub-agent error: denied by capability manifest (subagents: 0)"
                        .to_string();
                }
                match r1(task.to_string(), None) {
                    Ok(answer) => answer,
                    Err(e) => format!("sub-agent error: {e}"),
                }
            });
            let runner2 = Arc::clone(&runner);
            engine.register_fn("run_subagent", move |task: &str, model: &str| -> String {
                if !sub_ok {
                    return "sub-agent error: denied by capability manifest (subagents: 0)"
                        .to_string();
                }
                match runner2(task.to_string(), Some(model.to_string())) {
                    Ok(answer) => answer,
                    Err(e) => format!("sub-agent error: {e}"),
                }
            });

            let inbox = Arc::clone(&self.inbox);
            let spawn_runner = Arc::clone(&runner);
            engine.register_fn(
                "spawn_subagent",
                move |task: &str, model: &str, tag: &str| -> String {
                    if !sub_ok {
                        return "denied: capability manifest forbids sub-agents (subagents: 0)"
                            .to_string();
                    }
                    let runner = Arc::clone(&spawn_runner);
                    let inbox = Arc::clone(&inbox);
                    let task = task.to_string();
                    let model = if model.is_empty() {
                        None
                    } else {
                        Some(model.to_string())
                    };
                    let tag = tag.to_string();
                    let (job_id, _job) = pirs_agent::jobs::registry().register(
                        pirs_agent::jobs::JobKind::Agent,
                        task.chars().take(60).collect(),
                        std::env::temp_dir().join("pirs-subagent.log"),
                        None,
                    );
                    pirs_agent::jobs::registry().set_group(job_id, tag.clone());
                    let tag2 = tag.clone();
                    std::thread::spawn(move || {
                        let result =
                            runner(task, model).unwrap_or_else(|e| format!("sub-agent error: {e}"));
                        let status = if result.starts_with("sub-agent error") {
                            1
                        } else {
                            0
                        };
                        pirs_agent::jobs::registry()
                            .set_status(job_id, pirs_agent::jobs::JobStatus::Exited(status));
                        inbox.lock().unwrap().push((tag, result));
                    });
                    tag2
                },
            );
            let inbox2 = Arc::clone(&self.inbox);
            engine.register_fn("inbox", move || -> rhai::Array {
                let items: Vec<(String, String)> = std::mem::take(&mut *inbox2.lock().unwrap());
                items
                    .into_iter()
                    .map(|(tag, result)| {
                        let mut m = rhai::Map::new();
                        m.insert("tag".into(), tag.into());
                        m.insert("result".into(), result.into());
                        Dynamic::from_map(m)
                    })
                    .collect()
            });
        } else {
            // Always expose inbox() so packs like critic.rhai don't warn when
            // the host has no subagent runner (claw chat / unattended ticks).
            engine.register_fn("inbox", || -> rhai::Array { rhai::Array::new() });
        }

        let ast = engine
            .compile(source)
            .map_err(|e| anyhow!("parse error in {name}: {e}"))?;

        let has_fn = |name: &str| ast.iter_functions().any(|f| f.name == name);
        let has_on_tool_call = has_fn("on_tool_call");
        let has_on_tool_result = has_fn("on_tool_result");
        let has_on_context = has_fn("on_context");
        let has_on_should_stop = has_fn("on_should_stop");
        let has_on_steering = has_fn("on_steering");
        let has_on_follow_up = has_fn("on_follow_up");
        let has_on_event = has_fn("on_event");

        let mut scope = Scope::new();
        engine
            .run_ast_with_scope(&mut scope, &ast)
            .map_err(|e| anyhow!("error evaluating {name}: {e}"))?;

        let mut ast = ast;
        ast.clear_statements();
        if let Some(pm_runner) = runner_opt {
            let pm_ast = ast.clone();
            let pm_state = Arc::clone(&state);
            let pm_caps = caps.clone();
            let pm_sub_ok = caps::subagents_allowed(&caps);
            engine.register_fn(
                "parallel_map",
                move |items: rhai::Array,
                      concurrency: rhai::INT,
                      fn_name: &str,
                      model: &str|
                      -> rhai::Array {
                    if !pm_sub_ok {
                        return vec![Dynamic::from(
                            "denied: capability manifest forbids sub-agents (subagents: 0)",
                        )];
                    }
                    let conc = (concurrency.max(1) as usize).min(MAX_PARALLEL_MAP);
                    parallel_map_impl(
                        pm_ast.clone(),
                        pm_state.clone(),
                        pm_runner.clone(),
                        items,
                        conc,
                        fn_name,
                        model,
                        pm_caps.clone(),
                    )
                },
            );
        }

        let declared = registered.lock().unwrap().clone();

        let has_dispatch = ast.iter_functions().any(|f| f.name == "tool_dispatch");
        for (tool_name, description, schema_map) in declared {
            let fn_name = format!("tool_{tool_name}");
            let has_named = ast.iter_functions().any(|f| f.name == fn_name);
            if !has_named && !has_dispatch {
                return Err(anyhow!(
                    "{name}: register_tool(\"{tool_name}\") requires `fn {fn_name}(args)` or a `fn tool_dispatch(name, args)` fallback"
                ));
            }
            let schema = rhai::serde::from_dynamic(&Dynamic::from_map(schema_map))
                .unwrap_or(Value::Object(serde_json::Map::new()));
            self.tool_registry.push(RegisteredTool {
                name: tool_name.clone(),
                description,
                schema,
                ext: ext_index,
            });
        }

        for (cmd_name, description) in registered_cmds.lock().unwrap().clone() {
            let fn_name = format!("cmd_{cmd_name}");
            if !ast.iter_functions().any(|f| f.name == fn_name) {
                return Err(anyhow!(
                    "{name}: register_command(\"{cmd_name}\") requires a function `fn {fn_name}(args)`"
                ));
            }
            self.command_registry.push(RegisteredCommand {
                name: cmd_name,
                description,
                ext: ext_index,
            });
        }

        self.ext_flags.push(ExtFlags {
            has_on_tool_call,
            has_on_tool_result,
            has_on_context,
            has_on_should_stop,
            has_on_steering,
            has_on_follow_up,
            has_on_event,
        });
        self.extensions.push(Mutex::new(Extension {
            path: PathBuf::from(name),
            engine,
            ast,
            scope,
            caps,
        }));
        Ok(())
    }

    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn AgentTool>> {
        self.tool_registry
            .iter()
            .map(|t| {
                Arc::new(RhaiTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    schema: t.schema.clone(),
                    host: Arc::clone(self),
                    ext: t.ext,
                }) as Arc<dyn AgentTool>
            })
            .collect()
    }

    pub fn hooks(self: &Arc<Self>) -> Hooks {
        let mut hooks = Hooks::default();
        let has_call = self.ext_flags.iter().any(|f| f.has_on_tool_call);
        let has_result = self.ext_flags.iter().any(|f| f.has_on_tool_result);

        if has_call {
            let host = Arc::clone(self);
            hooks.before_tool_call = Some(Arc::new(move |id, name, args| {
                host.run_on_tool_call(id, name, args)
            }));
        }
        if has_result {
            let host = Arc::clone(self);
            hooks.after_tool_call = Some(Arc::new(move |id, name, result| {
                host.run_on_tool_result(id, name, result)
            }));
        }
        let has_context = self.ext_flags.iter().any(|f| f.has_on_context);
        if has_context {
            let host = Arc::clone(self);
            hooks.transform_context = Some(Arc::new(move |messages| host.run_on_context(messages)));
        }
        let has_stop = self.ext_flags.iter().any(|f| f.has_on_should_stop);
        if has_stop {
            let host = Arc::clone(self);
            hooks.should_stop_after_turn = Some(Arc::new(move |ctx| host.run_on_should_stop(ctx)));
        }
        let has_steering = self.ext_flags.iter().any(|f| f.has_on_steering);
        if has_steering {
            let host = Arc::clone(self);
            hooks.get_steering_messages = Some(Arc::new(move || host.run_on_steering()));
        }
        let has_follow = self.ext_flags.iter().any(|f| f.has_on_follow_up);
        if has_follow {
            let host = Arc::clone(self);
            hooks.get_follow_up_messages = Some(Arc::new(move || host.run_on_follow_up()));
        }
        hooks
    }

    fn run_on_tool_call(&self, id: &str, name: &str, args: &Value) -> Option<String> {
        for (i, ext) in self.extensions.iter().enumerate() {
            // Skip extensions without this hook WITHOUT locking — a busy
            // extension that has no policy hook must not block a concurrent
            // tool call.
            if !self.ext_flags[i].has_on_tool_call {
                continue;
            }
            // Policy hooks are a security gate: if the extension is busy
            // (re-entrant call from a hook that spawned a sub-agent on this
            // same host), a blocking lock would deadlock, so we try_lock and
            // FAIL CLOSED — unevaluated policy means deny.
            let mut ext = match ext.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    self.record_error(
                        "on_tool_call",
                        "extension busy (re-entrant); blocking tool call to avoid deadlock",
                    );
                    return Some(
                        "blocked: policy extension busy (re-entrant hook); cannot evaluate"
                            .to_string(),
                    );
                }
            };
            let dynamic_args = rhai::serde::to_dynamic(args).unwrap_or(Dynamic::UNIT);
            let ext = &mut *ext;
            let result: Result<Dynamic, _> = ext.engine.call_fn(
                &mut ext.scope,
                &ext.ast,
                "on_tool_call",
                (id.to_string(), name.to_string(), dynamic_args),
            );
            match result {
                Ok(d) if d.is_unit() => continue,
                Ok(d) => {
                    if d.is::<rhai::Map>() {
                        let map = d.cast::<rhai::Map>();
                        let block = map
                            .get("block")
                            .and_then(|b| b.as_bool().ok())
                            .unwrap_or(false);
                        if block {
                            let reason = map
                                .get("reason")
                                .map(|r| r.to_string())
                                .unwrap_or_else(|| "blocked by extension".to_string());
                            return Some(reason);
                        }
                    }
                }
                Err(e) => {
                    // A policy hook is a security gate: a script error (bad args,
                    // op-limit, thrown error) means the policy could not be
                    // evaluated, so FAIL CLOSED — deny rather than silently run
                    // the tool. Consistent with the busy-lock branch above.
                    let path = ext.path.display().to_string();
                    tracing::warn!("on_tool_call in {path} failed: {e}");
                    self.record_error("on_tool_call", format!("{path}: {e}"));
                    return Some(format!(
                        "blocked: policy hook in {path} errored; cannot evaluate ({e})"
                    ));
                }
            }
        }
        None
    }

    fn run_on_tool_result(
        &self,
        id: &str,
        name: &str,
        result: &ToolResultMessage,
    ) -> Option<ToolResultPatch> {
        for (i, ext) in self.extensions.iter().enumerate() {
            // Skip extensions without this hook WITHOUT locking, so a busy
            // extension with no result hook doesn't cause us to record a
            // spurious error / skip. Only lock ones that actually patch results.
            if !self.ext_flags[i].has_on_tool_result {
                continue;
            }
            // After-hooks observe/patch results; they are not gates. On
            // re-entrant contention skip with a recorded error rather than
            // deadlock (a blocking lock here hangs the host).
            let mut ext = match ext.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    self.record_error(
                        "on_tool_result",
                        "extension busy (re-entrant); result hook skipped to avoid deadlock",
                    );
                    continue;
                }
            };
            let text: String = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            let mut map = rhai::Map::new();
            map.insert("text".into(), text.into());
            map.insert("isError".into(), result.is_error.into());
            map.insert("terminate".into(), result.terminate.into());
            if let Some(d) = &result.details {
                map.insert(
                    "details".into(),
                    rhai::serde::to_dynamic(d).unwrap_or(Dynamic::UNIT),
                );
            }
            let ext = &mut *ext;
            let call_result: Result<Dynamic, _> = ext.engine.call_fn(
                &mut ext.scope,
                &ext.ast,
                "on_tool_result",
                (id.to_string(), name.to_string(), Dynamic::from_map(map)),
            );
            match call_result {
                Ok(d) if d.is_unit() => continue,
                Ok(d) => {
                    if !d.is::<rhai::Map>() {
                        continue;
                    }
                    let map = d.cast::<rhai::Map>();
                    let patch = ToolResultPatch {
                        content: map
                            .get("text")
                            .map(|t| vec![ContentBlock::text(t.to_string())]),
                        details: map.get("details").and_then(|d| {
                            if d.is_unit() {
                                None
                            } else {
                                rhai::serde::from_dynamic(d).ok()
                            }
                        }),
                        is_error: map.get("isError").and_then(|b| b.as_bool().ok()),
                        terminate: map.get("terminate").and_then(|b| b.as_bool().ok()),
                    };
                    return Some(patch);
                }
                Err(e) => {
                    tracing::warn!("on_tool_result in {} failed: {e}", ext.path.display());
                }
            }
        }
        None
    }

    pub fn commands(&self) -> Vec<(String, String)> {
        self.command_registry
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect()
    }

    pub fn run_command(&self, name: &str, args: &str) -> Result<String, String> {
        // Last registration wins (matches tool last-wins / project overrides).
        let Some(cmd) = self.command_registry.iter().rev().find(|c| c.name == name) else {
            return Err(format!("unknown command: {name}"));
        };
        let fn_name = format!("cmd_{name}");
        // Commands must not silently no-op when the extension lock is busy
        // (hooks skip; slash UX must report the miss).
        let mut guard = match self.extensions[cmd.ext].try_lock() {
            Ok(g) => g,
            Err(_) => {
                return Err(format!("{name}: extension busy; retry"));
            }
        };
        let ext = &mut *guard;
        let result: Dynamic = ext
            .engine
            .call_fn(&mut ext.scope, &ext.ast, &fn_name, (args.to_string(),))
            .map_err(|e| e.to_string())?;
        Ok(if result.is_unit() {
            String::new()
        } else if result.is::<String>() {
            result.cast::<String>()
        } else {
            result.to_string()
        })
    }

    pub fn extension_names(&self) -> Vec<String> {
        self.extensions
            .iter()
            .map(|e| e.lock().unwrap().path.display().to_string())
            .collect()
    }

    pub fn listener(self: &Arc<Self>) -> Option<pirs_agent::Emit> {
        let any = self.ext_flags.iter().any(|f| f.has_on_event);
        if !any {
            return None;
        }
        let host = Arc::clone(self);
        Some(Arc::new(move |event: pirs_agent::AgentEvent| {
            host.dispatch_event(&event);
        }))
    }

    /// Test hook: invoke one extension's function directly.
    #[doc(hidden)]
    pub fn call_extension_for_test(
        &self,
        ext_index: usize,
        fn_name: &str,
        args: impl rhai::FuncArgs + Send,
    ) -> Result<Dynamic, String> {
        self.call_extension(ext_index, fn_name, args)
    }

    fn call_extension(
        &self,
        ext_index: usize,
        fn_name: &str,
        args: impl rhai::FuncArgs + Send,
    ) -> Result<Dynamic, String> {
        // Reentrancy guard: a hook that spawns a sub-agent whose policy hooks
        // land on this same extension must not deadlock. If the lock is held,
        // skip the hook (the parent's hook is the policy already evaluating).
        let mut guard = match self.extensions[ext_index].try_lock() {
            Ok(g) => g,
            Err(_) => {
                self.record_error(
                    fn_name,
                    "hook skipped: extension re-entered while already running (deadlock prevented)",
                );
                return Ok(Dynamic::UNIT);
            }
        };
        let ext = &mut *guard;
        ext.engine
            .call_fn(&mut ext.scope, &ext.ast, fn_name, args)
            .map_err(|e| e.to_string())
    }

    fn for_each_with(&self, flag: ExtensionFlag, mut f: impl FnMut(&Self, usize)) {
        for i in 0..self.extensions.len() {
            // Read the hook flag without locking the extension (a blocking lock
            // here would hang if the extension is mid-run).
            let e = &self.ext_flags[i];
            let has = match flag {
                ExtensionFlag::Context => e.has_on_context,
                ExtensionFlag::ShouldStop => e.has_on_should_stop,
                ExtensionFlag::Steering => e.has_on_steering,
                ExtensionFlag::FollowUp => e.has_on_follow_up,
                ExtensionFlag::Event => e.has_on_event,
            };
            if has {
                f(self, i);
            }
        }
    }

    fn run_on_context(&self, messages: Vec<pirs_ai::Message>) -> Vec<pirs_ai::Message> {
        let mut current = messages;
        self.for_each_with(ExtensionFlag::Context, |host, i| {
            let json = serde_json::to_value(&current).unwrap_or_else(|_| Value::Array(vec![]));
            let arg = rhai::serde::to_dynamic(&json).unwrap_or(Dynamic::UNIT);
            match host.call_extension(i, "on_context", (arg,)) {
                Ok(d) if d.is_unit() => {}
                Ok(d) => {
                    let parsed: Result<Value, _> = rhai::serde::from_dynamic(&d);
                    match parsed {
                        Ok(v) => match serde_json::from_value::<Vec<pirs_ai::Message>>(v) {
                            Ok(msgs) => current = msgs,
                            Err(e) => tracing::warn!("on_context returned invalid messages: {e}"),
                        },
                        Err(e) => {
                            self.record_error("on_context", format!("returned non-JSON value: {e}"))
                        }
                    }
                }
                Err(e) => self.record_error("on_context", e),
            }
        });
        current
    }

    fn run_on_should_stop(&self, ctx: &pirs_ai::Context) -> bool {
        let mut stop = false;
        self.for_each_with(ExtensionFlag::ShouldStop, |host, i| {
            if stop {
                return;
            }
            let json = serde_json::to_value(&ctx.messages).unwrap_or_else(|_| Value::Array(vec![]));
            let mut map = rhai::Map::new();
            map.insert(
                "messages".into(),
                rhai::serde::to_dynamic(&json).unwrap_or(Dynamic::UNIT),
            );
            match host.call_extension(i, "on_should_stop", (Dynamic::from_map(map),)) {
                Ok(d) => {
                    stop = d.as_bool().unwrap_or(false);
                }
                Err(e) => self.record_error("on_should_stop", e),
            }
        });
        stop
    }

    fn run_on_steering(&self) -> Vec<pirs_ai::Message> {
        let mut out = Vec::new();
        self.for_each_with(ExtensionFlag::Steering, |host, i| {
            match host.call_extension(i, "on_steering", ()) {
                Ok(d) => out.extend(convert::dynamic_to_messages(&d)),
                Err(e) => self.record_error("on_steering", e),
            }
        });
        out
    }

    fn run_on_follow_up(&self) -> Vec<pirs_ai::Message> {
        let mut out = Vec::new();
        self.for_each_with(ExtensionFlag::FollowUp, |host, i| {
            match host.call_extension(i, "on_follow_up", ()) {
                Ok(d) => out.extend(convert::dynamic_to_messages(&d)),
                Err(e) => self.record_error("on_follow_up", e),
            }
        });
        out
    }

    fn dispatch_event(&self, event: &pirs_agent::AgentEvent) {
        let (ty, data) = convert::event_to_rhai(event);
        self.for_each_with(ExtensionFlag::Event, |host, i| {
            if let Err(e) = host.call_extension(i, "on_event", (ty.clone(), data.clone())) {
                host.record_error("on_event", e);
            }
        });
    }
}

fn worker_engine(state: &StateStore, runner: &SubagentRunner, caps: &caps::Caps) -> Engine {
    let mut engine = build_engine(state, caps);
    let r1 = runner.clone();
    engine.register_fn("run_subagent", move |task: &str| -> String {
        match r1(task.to_string(), None) {
            Ok(a) => a,
            Err(e) => format!("sub-agent error: {e}"),
        }
    });
    let r2 = runner.clone();
    engine.register_fn("run_subagent", move |task: &str, model: &str| -> String {
        match r2(task.to_string(), Some(model.to_string())) {
            Ok(a) => a,
            Err(e) => format!("sub-agent error: {e}"),
        }
    });
    engine
}

/// Hard cap on concurrent native threads spawned by `parallel_map` (M-8).
pub const MAX_PARALLEL_MAP: usize = 32;

/// Clamp parallel_map concurrency for tests and host registration.
pub fn clamp_parallel_concurrency(requested: usize) -> usize {
    requested.max(1).min(MAX_PARALLEL_MAP)
}

#[allow(clippy::too_many_arguments)]
fn parallel_map_impl(
    ast: AST,
    state: StateStore,
    runner: SubagentRunner,
    items: rhai::Array,
    concurrency: usize,
    fn_name: &str,
    model: &str,
    caps: caps::Caps,
) -> rhai::Array {
    let concurrency = clamp_parallel_concurrency(concurrency);
    let mut results: Vec<Dynamic> = vec![Dynamic::UNIT; items.len()];
    let mut idx = 0usize;
    while idx < items.len() {
        let end = (idx + concurrency).min(items.len());
        let mut handles = Vec::new();
        for (i, item) in items[idx..end].iter().enumerate() {
            let ast = ast.clone();
            let state = state.clone();
            let runner = runner.clone();
            let item = item.clone();
            let fn_name = fn_name.to_string();
            let caps = caps.clone();
            let model = if model.is_empty() {
                None
            } else {
                Some(model.to_string())
            };
            handles.push((
                idx + i,
                std::thread::spawn(move || {
                    if fn_name.is_empty() {
                        match runner(item.to_string(), model) {
                            Ok(answer) => Dynamic::from(answer),
                            Err(e) => Dynamic::from(format!("sub-agent error: {e}")),
                        }
                    } else {
                        let engine = worker_engine(&state, &runner, &caps);
                        let mut scope = Scope::new();
                        match engine.call_fn::<Dynamic>(&mut scope, &ast, &fn_name, (item,)) {
                            Ok(d) => d,
                            Err(e) => Dynamic::from(format!("__error__: {e}")),
                        }
                    }
                }),
            ));
        }
        for (i, h) in handles {
            results[i] = h
                .join()
                .unwrap_or_else(|_| Dynamic::from("worker panicked"));
        }
        idx = end;
    }
    results
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]


enum ExtensionFlag {
    Context,
    ShouldStop,
    Steering,
    FollowUp,
    Event,
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

struct RhaiTool {
    name: String,
    description: String,
    schema: Value,
    host: Arc<ExtensionHost>,
    ext: usize,
}

#[async_trait::async_trait]
impl AgentTool for RhaiTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let host = Arc::clone(&self.host);
        let ext_index = self.ext;
        let fn_name = format!("tool_{}", self.name);
        let args = ctx.args.clone();

        let output = tokio::task::spawn_blocking(move || {
            // try_lock: a tool fn that spawns a sub-agent re-entering this
            // extension would deadlock on a blocking lock.
            let mut ext_guard = match host.extensions[ext_index].try_lock() {
                Ok(g) => g,
                Err(_) => anyhow::bail!(
                    "extension busy (re-entrant call); refusing to run tool to avoid deadlock"
                ),
            };
            let ext = &mut *ext_guard;
            let dynamic_args = rhai::serde::to_dynamic(&args).unwrap_or(Dynamic::UNIT);
            let result: Result<Dynamic, _> = if ext.ast.iter_functions().any(|f| f.name == fn_name)
            {
                ext.engine
                    .call_fn(&mut ext.scope, &ext.ast, &fn_name, (dynamic_args,))
            } else {
                ext.engine.call_fn(
                    &mut ext.scope,
                    &ext.ast,
                    "tool_dispatch",
                    (
                        fn_name.trim_start_matches("tool_").to_string(),
                        dynamic_args,
                    ),
                )
            };
            result.map_err(|e| anyhow!("{e}"))
        })
        .await??;

        let text = if output.is_unit() {
            String::new()
        } else if output.is::<String>() {
            output.cast::<String>()
        } else if output.is::<rhai::Map>() || output.is::<rhai::Array>() {
            let json: Value = rhai::serde::from_dynamic(&output)?;
            serde_json::to_string_pretty(&json)?
        } else {
            output.to_string()
        };
        Ok(ToolOutput::text(text))
    }
}

pub use trust::{trust_directory, TrustDecision};

#[cfg(test)]
mod host_api_tests {
    use super::*;

    #[test]
    fn core_host_apis_checkpoint_create_restores() {
        register_core_host_apis();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("f.txt"), b"v1").unwrap();
        let mut created = None;
        for (name, f) in QUERY_FNS.read().unwrap().iter() {
            if name == "checkpoint_create" {
                let rows = f("auto-test");
                assert!(!rows.is_empty(), "{rows:?}");
                assert!(!rows[0].starts_with("error:"), "{rows:?}");
                created = Some(rows[0].clone());
            }
        }
        let id = created.expect("checkpoint_create registered");
        std::fs::write(dir.path().join("f.txt"), b"dirty").unwrap();
        for (name, f) in QUERY_FNS.read().unwrap().iter() {
            if name == "checkpoint_restore" {
                let rows = f(&id);
                assert!(rows[0].contains("restored"), "{rows:?}");
            }
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "v1");
        std::env::set_current_dir(prev).unwrap();
    }

    #[test]
    fn core_host_apis_project_profile_on_this_repo() {
        register_core_host_apis();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let mut found = false;
        for (name, f) in QUERY_FNS.read().unwrap().iter() {
            if name == "project_profile" {
                let lines = f(root.to_str().unwrap());
                assert!(
                    lines.iter().any(|l| l.contains("cargo") || l.starts_with("test=")),
                    "project_profile lines: {lines:?}"
                );
                found = true;
            }
        }
        assert!(found, "project_profile query not registered");
    }
}


#[cfg(test)]
mod parallel_map_clamp_tests {
    use super::{clamp_parallel_concurrency, MAX_PARALLEL_MAP};

    #[test]
    fn parallel_map_concurrency_is_capped() {
        assert_eq!(clamp_parallel_concurrency(0), 1);
        assert_eq!(clamp_parallel_concurrency(1), 1);
        assert_eq!(clamp_parallel_concurrency(8), 8);
        assert_eq!(clamp_parallel_concurrency(10_000), MAX_PARALLEL_MAP);
        assert!(MAX_PARALLEL_MAP <= 64);
        // Registration site uses MAX_PARALLEL_MAP
        let src = include_str!("lib.rs");
        assert!(src.contains("MAX_PARALLEL_MAP"));
        assert!(src.contains("clamp_parallel_concurrency"));
    }
}
