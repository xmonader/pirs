use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use clap::{CommandFactory, FromArgMatches, Parser};
use pirs_agent::{Agent, AgentEvent, AgentTool, Hooks};
use pirs_ai::{CompletionOptions, Message, OpenAiCompat};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

mod acp_mode;
mod approval;
mod auth;
mod blame;
mod config_file;
mod discovery;
mod pack;
mod replay;
mod rpc_mode;
mod serve;
mod session;
mod subagent;
mod system_prompt;
mod tui;
mod observability;
mod registry;
mod session_stats;
mod weak_compose;

#[derive(Parser)]
#[command(
    name = "pirs",
    about = "Rust port of the pi coding agent, extensible via rhai"
)]
struct Cli {
    /// One-shot prompt; if omitted, starts the interactive REPL.
    /// Collects all trailing args so pseudo-subcommands work unquoted
    /// (`pirs blame src/main.rs:42`, `pirs pack install <url> --yes`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    prompt: Vec<String>,

    /// Run mode: interactive REPL, TUI, headless JSONL-over-stdio RPC, or
    /// ACP (Agent Client Protocol, JSON-RPC 2.0 over stdio — for editors
    /// like Zed that embed agents directly)
    #[arg(long, default_value = "repl")]
    mode: String,

    /// Model id to use
    #[arg(short, long, env = "PIRS_MODEL", default_value = "gpt-4o-mini")]
    model: String,

    /// LLM provider: openai (OpenAI-compatible) or anthropic
    #[arg(long, env = "PIRS_PROVIDER", default_value = "openai")]
    provider: String,

    /// OpenAI-compatible base URL
    #[arg(long, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    /// API key (falls back to the provider's auth store or env var)
    #[arg(long)]
    api_key: Option<String>,

    /// Resume the most recent session for this directory
    #[arg(long)]
    resume: bool,

    /// Run a multi-phase loop strategy for a one-shot prompt. Primary built-ins:
    /// `monolithic`, `plan-exec`, `plan-critic-exec` (alias `plan-exec-critic`).
    /// Also accepts other built-in names, `.pirs/strategies/<name>.rhai`, or a
    /// path to a .rhai script. No effect in the interactive REPL.
    /// Pair with `--plan-model` for strong-plan / weak-exec multi-model runs.
    #[arg(long)]
    strategy: Option<String>,

    /// Model for planning (and critique) phases only. Executor phases use
    /// `--model`. Enables the strong-planner / weak-executor pitch without
    /// editing strategy scripts, e.g.:
    ///   pirs --model cheap-model --plan-model strong-model --strategy plan-exec "…"
    #[arg(long)]
    plan_model: Option<String>,

    /// Run under a profile (a role: persona + model + strategy + tool policy).
    /// Accepts a name resolved from .pirs/profiles/<name>.rhai (project then
    /// ~/.pirs), a built-in name (`weak`), or a path to a .rhai script. Implies
    /// its strategy; --strategy overrides which strategy the profile runs.
    #[arg(long)]
    profile: Option<String>,

    /// Shell command that verifies a strategy attempt succeeded (exit 0 = pass,
    /// e.g. "cargo test" or "pytest -x"). On failure its output is fed into the
    /// next attempt as the prior verdict, so the strategy re-plans against the
    /// real error. Only used with --strategy/--profile.
    #[arg(long)]
    verify: Option<String>,

    /// Max strategy attempts when --verify is set (retry on gate failure).
    /// Defaults to 3 with --verify, 1 otherwise.
    #[arg(long)]
    max_attempts: Option<u32>,

    /// Skip injecting the PageRank repo-map sketch into the system prompt
    /// (on by default when the code graph is enabled).
    #[arg(long)]
    no_repo_map: bool,

    /// Disable rhai extension loading
    #[arg(long)]
    no_extensions: bool,

    /// Disable MCP server connections (.mcp.json)
    #[arg(long)]
    no_mcp: bool,

    /// Approval mode: auto (no prompts), ask (inline y/n for sensitive ops), yolo (no prompts, no policy hooks — dangerous)
    #[arg(long, env = "PIRS_APPROVAL", default_value = "auto")]
    approval: String,

    /// Safety profile (Vibe-class): default | plan | accept-edits | auto-approve
    /// Enforced on every tool call (plan = read-only; accept-edits auto-allows file tools;
    /// auto-approve skips approval prompts for all tools).
    #[arg(long = "agent-profile", env = "PIRS_AGENT_PROFILE", default_value = "default")]
    agent_profile: String,

    /// Run this session inside a git worktree for the named branch (create or reuse
    /// under `.pirs/worktrees/<name>`). Session cwd becomes that worktree.
    #[arg(long, env = "PIRS_WORKTREE")]
    worktree: Option<String>,

    /// Retry failed/rate-limited requests up to N times
    #[arg(long, default_value = "0")]
    max_retries: u32,

    /// Disable automatic context compaction
    #[arg(long)]
    no_compaction: bool,

    /// Model context window in tokens (drives compaction threshold)
    #[arg(long, default_value = "128000")]
    context_window: u64,

    /// Disable the code graph (code_map/ast_edit tools, blast-radius notes)
    #[arg(long)]
    no_graph: bool,

    /// Cache the code graph in .pirs/graph.db and refresh it incrementally
    /// (re-parse only changed files). Speeds up warm starts on large repos;
    /// off by default. The cache is disposable and never source of truth.
    #[arg(long)]
    persist_graph: bool,

    /// Enable the semantic_search tool: natural-language code search via an
    /// embedding service (implies --persist-graph for the vector store). Point
    /// it at any OpenAI-compatible /v1/embeddings endpoint with the flags below.
    #[arg(long)]
    semantic: bool,

    /// Embeddings endpoint base URL (OpenAI-compatible), e.g. Ollama's
    /// http://localhost:11434/v1 [env: PIRS_EMBED_BASE_URL]
    #[arg(long, env = "PIRS_EMBED_BASE_URL")]
    embed_base_url: Option<String>,

    /// Embedding model id [env: PIRS_EMBED_MODEL]
    #[arg(long, env = "PIRS_EMBED_MODEL")]
    embed_model: Option<String>,

    /// API key for the embeddings endpoint (optional for local servers)
    /// [env: PIRS_EMBED_API_KEY]
    #[arg(long, env = "PIRS_EMBED_API_KEY")]
    embed_api_key: Option<String>,

    /// Max source chars embedded per symbol. Lower it for small-context models
    /// (e.g. 512 for all-minilm) to avoid the truncating fallback; big-context
    /// models can leave the default [env: PIRS_EMBED_MAX_CHARS]
    #[arg(long, env = "PIRS_EMBED_MAX_CHARS")]
    embed_max_chars: Option<usize>,

    /// Opt into SYNCHRONOUS inline embedding instead of the default background
    /// indexer: code_search embeds up to N symbols per call (and no background
    /// task runs). Useful for a one-shot that must build the index in-process.
    /// By default, indexing runs in the background and searches never block.
    #[arg(long, env = "PIRS_EMBED_BATCH_CAP")]
    embed_batch_cap: Option<usize>,

    /// Start with only core tools loaded; model loads more via use_tool
    #[arg(long)]
    tool_diet: bool,

    /// Execute tool calls one at a time (helps weaker models)
    #[arg(long)]
    sequential: bool,

    /// Weak-model hardening preset: enables --tool-diet, --sequential,
    /// --max-retries at least 3, defaults --strategy to plan-exec when
    /// neither --strategy nor --profile is set, loads bundled packs
    /// (weak-model, context-janitor, env-doctor, goal), and auto-sets
    /// --verify from the project test ecosystem when possible. Multi-model:
    /// pair with `--plan-model <strong>` so planning stays strong while this
    /// run's `--model` is the weak executor; or use phase `model:` / `--cascade`.
    #[arg(long)]
    weak: bool,

    /// Draft each turn with a cheaper model; escalate to the main model only when the draft is rejected
    #[arg(long)]
    cascade: Option<String>,

    /// JSONL flight recorder for this run (agent events + strategy phases).
    /// Omit PATH to write `~/.pirs/traces/<session>-<ts>-<pid>.jsonl`.
    /// Same schema as `pirs-bench --trace` (jq-friendly, crash-safe).
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "AUTO")]
    trace: Option<String>,

    /// Max agent turns (exit code 53 when hit)
    #[arg(long)]
    max_turns: Option<usize>,

    /// Max wall-clock seconds (exit code 54 when hit)
    #[arg(long)]
    max_wall_time: Option<u64>,

    /// Max tool calls (exit code 55 when hit)
    #[arg(long)]
    max_tool_calls: Option<usize>,

    /// Run the local web app (pirs serve): browser UI on localhost
    #[arg(long)]
    serve: bool,

    /// Port for --serve
    #[arg(long, default_value = "8477")]
    port: u16,

    /// Bind address for --serve
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Auth token for --serve writes (default: generated per run)
    #[arg(long, env = "PIRS_SERVE_TOKEN")]
    serve_token: Option<String>,

    /// Allow --serve to bind non-loopback addresses
    #[arg(long)]
    serve_external: bool,

    /// Print how model/provider/base-url/approval were each resolved (cli
    /// flag / env var / project config / user config / default) and exit.
    #[arg(long)]
    show_config: bool,

    /// Print runtime doctor report (API keys present, toolchain, LSP, MCP,
    /// git, browser/CDP, computer-use, gh, soul/audit) and exit.
    #[arg(long)]
    doctor: bool,

    /// Permission ladder: read-only | workspace-write | danger-full-access
    /// (composes with --agent-profile). Env: PIRS_PERMISSION_MODE.
    #[arg(long = "permission-mode", env = "PIRS_PERMISSION_MODE")]
    permission_mode: Option<String>,

    /// Product dial: plan (read-only tools) or act (full tools). Sets
    /// permission mode + agent-profile when those flags are left default.
    #[arg(long = "mode-dial", env = "PIRS_MODE_DIAL")]
    mode_dial: Option<String>,
}

struct Printer {
    streaming: Mutex<bool>,
}

impl Printer {
    fn new() -> Self {
        Printer {
            streaming: Mutex::new(false),
        }
    }

    fn event(&self, event: AgentEvent) {
        let mut streaming = self.streaming.lock().unwrap();
        match event {
            AgentEvent::MessageUpdate { .. } => {}
            AgentEvent::MessageStart { message } => {
                if let Message::Assistant(_) = &*message {
                    *streaming = true;
                }
            }
            AgentEvent::MessageEnd { message } => {
                if let Message::Assistant(a) = &*message {
                    if *streaming {
                        println!();
                        *streaming = false;
                    }
                    if a.stop_reason == pirs_ai::StopReason::Error {
                        eprintln!(
                            "\n[error: {}]",
                            a.error_message.as_deref().unwrap_or("unknown")
                        );
                    }
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                let summary = summarize_args(&tool_name, &args);
                println!("\n\x1b[2m> {tool_name} {summary}\x1b[0m");
            }
            AgentEvent::ToolExecutionEnd { result, .. } => {
                // Prefer details.uiText (full) over model-capped content for display.
                let text = result.display_text();
                let preview: String = text.lines().take(6).collect::<Vec<_>>().join("\n");
                let marker = if result.is_error { "x" } else { "-" };
                if !preview.is_empty() {
                    println!("\x1b[2m{marker} {preview}\x1b[0m");
                }
            }
            _ => {}
        }
    }
}

impl Default for Printer {
    fn default() -> Self {
        Self::new()
    }
}

/// Installs the approval gate as the before-tool hook when nothing else claimed
/// the slot. Yolo mode explicitly waives this install (interactive approval);
/// safety-profile denials under yolo are chained separately via
/// [`chain_gate_with_extensions`] / [`install_profile_under_yolo_if_needed`].
fn install_gate_if_absent(
    hooks: &mut pirs_agent::Hooks,
    gate_hook: &Option<pirs_agent::events::BeforeToolCallHook>,
    approval: &str,
) {
    let yolo = approval::ApprovalMode::parse(approval) == Some(approval::ApprovalMode::Yolo);
    if !yolo && hooks.before_tool_call.is_none() {
        hooks.before_tool_call = gate_hook.clone();
    }
}

/// Under yolo, still enforce non-default `--agent-profile` hard denials when no
/// before_tool hook was installed (e.g. `--no-extensions`).
fn install_profile_under_yolo_if_needed(
    hooks: &mut pirs_agent::Hooks,
    gate_hook: &Option<pirs_agent::events::BeforeToolCallHook>,
    approval: &str,
    safety: pirs_tools::SafetyProfile,
) {
    let yolo = approval::ApprovalMode::parse(approval) == Some(approval::ApprovalMode::Yolo);
    if yolo
        && safety != pirs_tools::SafetyProfile::Default
        && hooks.before_tool_call.is_none()
    {
        hooks.before_tool_call = gate_hook.clone();
    }
}

/// Chain approval/profile gate with extension before_tool hooks.
/// Under yolo, skip the interactive approval gate unless a non-default safety
/// profile requires hard denials (then chain gate first, then extensions).
fn chain_gate_with_extensions(
    gate_hook: Option<pirs_agent::events::BeforeToolCallHook>,
    ext_before: Option<pirs_agent::events::BeforeToolCallHook>,
    yolo: bool,
    safety: pirs_tools::SafetyProfile,
) -> Option<pirs_agent::events::BeforeToolCallHook> {
    if yolo && safety == pirs_tools::SafetyProfile::Default {
        // Pure yolo: extensions only (no interactive approval gate).
        return ext_before;
    }
    // ask/auto, or yolo+profile: gate first (profile denials / prompts), then ext.
    pirs_agent::Hooks::chain_before(gate_hook, ext_before)
}

fn print_usage(report: &pirs_agent::usage::UsageReport) {
    let total = report.grand_total();
    let hit_rate = if total.input + total.cache_read > 0 {
        100.0 * total.cache_read as f64 / (total.input + total.cache_read) as f64
    } else {
        0.0
    };
    eprintln!(
        "[usage: {} api calls + {} delegate sub-agents | input {} (cached {}, {:.0}%) | output {} | reasoning {} | total {}]",
        report.calls.len() - report.delegate_calls(),
        report.delegate_calls(),
        total.input,
        total.cache_read,
        hit_rate,
        total.output,
        total.reasoning,
        total.total_tokens,
    );
    // Per-model lines make strong-plan / weak-exec splits visible at a glance.
    for (model, u) in &report.by_model {
        let calls = report.calls.iter().filter(|c| c.model == *model).count();
        eprintln!(
            "  {model} ({calls} call{}): input {} (cached {}) output {} total {}",
            if calls == 1 { "" } else { "s" },
            u.input,
            u.cache_read,
            u.output,
            u.total_tokens
        );
    }
}

fn summarize_args(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        "grep" | "find" => "pattern",
        _ => "",
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| {
            let s = s.replace('\n', " ");
            if s.chars().count() > 80 {
                format!("{}...", s.chars().take(80).collect::<String>())
            } else {
                s
            }
        })
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Parsed via ArgMatches (rather than plain `Cli::parse()`) so
    // `value_source()` can tell a value the user actually typed/exported
    // apart from one that just fell through to clap's hardcoded default —
    // that distinction is what lets project/user config.toml layers fill in
    // underneath CLI/env without ever overriding something the user set.
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    // Flatten trailing args into the single prompt string the rest of main
    // (and the pseudo-subcommands) expect.
    let cli = Cli {
        prompt: {
            let parts = std::mem::take(&mut cli.prompt);
            if parts.is_empty() {
                Vec::new()
            } else {
                vec![parts.join(" ")]
            }
        },
        ..cli
    };

    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Optional git worktree bind (Vibe --worktree class) before tools use cwd.
    if let Some(ref wt) = cli.worktree.clone() {
        match pirs_tools::bind_session_worktree(&cwd, &wt) {
            Ok(sess) => {
                eprintln!(
                    "[worktree: branch={} cwd={} created={}]",
                    sess.branch,
                    sess.cwd.display(),
                    sess.created
                );
                if let Err(e) = std::env::set_current_dir(&sess.cwd) {
                    eprintln!("[worktree: set_current_dir failed: {e}]");
                } else {
                    cwd = sess.cwd;
                }
            }
            Err(e) => {
                anyhow::bail!("--worktree {wt:?}: {e}");
            }
        }
    }
    let (project_cfg, user_cfg) = config_file::load_layers(&cwd);
    // `base_url`/`approval` are security-relevant (redirect API traffic /
    // disable the approval gate) so they are deliberately NEVER read from the
    // project layer — a `git clone`d repo's own .pirs/config.toml must not be
    // able to silently point requests at an attacker's endpoint or turn off
    // approval prompts just by being checked out. model/provider are inert
    // preferences and stay project-configurable. See config_file::FileConfig.
    if project_cfg.base_url.is_some() || project_cfg.approval.is_some() {
        eprintln!(
            "[note: project .pirs/config.toml sets base_url/approval, which are user-config-only and were ignored]"
        );
    }
    let project_cfg = config_file::restrict_project_layer(project_cfg);
    let (model, model_src) = config_file::resolve_str(
        &matches,
        "model",
        &cli.model,
        project_cfg.model.as_deref(),
        user_cfg.model.as_deref(),
    );
    let (provider, provider_src) = config_file::resolve_str(
        &matches,
        "provider",
        &cli.provider,
        project_cfg.provider.as_deref(),
        user_cfg.provider.as_deref(),
    );
    let (base_url, base_url_src) = config_file::resolve_opt(
        &matches,
        "base_url",
        cli.base_url.clone(),
        project_cfg.base_url.as_deref(),
        user_cfg.base_url.as_deref(),
    );
    let (approval, approval_src) = config_file::resolve_str(
        &matches,
        "approval",
        &cli.approval,
        project_cfg.approval.as_deref(),
        user_cfg.approval.as_deref(),
    );
    if cli.show_config {
        println!("model:      {model:<24} ({})", model_src.label());
        println!("provider:   {provider:<24} ({})", provider_src.label());
        println!(
            "base_url:   {:<24} ({})",
            base_url.as_deref().unwrap_or("(none)"),
            base_url_src.label()
        );
        println!("approval:   {approval:<24} ({})", approval_src.label());
        return Ok(());
    }
    if cli.doctor {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        for line in pirs_tools::doctor_report(&cwd) {
            println!("{line}");
        }
        return Ok(());
    }
    let mut cli = Cli {
        model,
        provider,
        base_url,
        approval,
        ..cli
    };
    // --weak: compose the recommended weak-model preset without requiring the
    // user to remember every flag. Pure rules live in weak_compose (unit-tested).
    if cli.weak {
        let detected = if cli.verify.is_none() && !cli.prompt.is_empty() {
            pirs_tools::run_tests::detect_verify_command(&cwd)
        } else {
            None
        };
        let composed = weak_compose::apply_weak_preset(
            weak_compose::WeakComposeInput {
                has_prompt: !cli.prompt.is_empty(),
                strategy: cli.strategy.clone(),
                profile: cli.profile.clone(),
                verify: cli.verify.clone(),
                max_retries: cli.max_retries,
                tool_diet: cli.tool_diet,
                sequential: cli.sequential,
            },
            detected,
        );
        cli.tool_diet = composed.tool_diet;
        cli.sequential = composed.sequential;
        cli.max_retries = composed.max_retries;
        cli.strategy = composed.strategy;
        cli.profile = composed.profile;
        cli.verify = composed.verify;
        if let Some(note) = &composed.auto_verify_note {
            eprintln!("{note}");
        }
        eprintln!(
            "[weak mode: tool-diet, sequential, max-retries={}, strategy={:?}, \
             verify={:?}, packs={:?}; multi-model: phase `model:` in strategies and/or --cascade <draft>]",
            cli.max_retries,
            cli.strategy.as_deref().or(cli.profile.as_deref()),
            cli.verify.as_deref(),
            composed.bundled_packs,
        );
    }

    if let Some(dir) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "trust" || p.starts_with("trust "))
    {
        let arg = dir.trim_start_matches("trust").trim().to_string();
        let target = if arg.is_empty() {
            std::env::current_dir()?
        } else {
            std::path::PathBuf::from(arg)
        };
        return match pirs_rhai::trust_directory(&target) {
            Ok(()) => {
                println!("trusted {}", target.display());
                Ok(())
            }
            Err(e) => anyhow::bail!(e),
        };
    }
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "replay" || p.starts_with("replay "))
    {
        // pirs replay <session.jsonl> [--model X]
        let args: Vec<&str> = spec
            .trim_start_matches("replay")
            .split_whitespace()
            .collect();
        let Some(file) = args.first().copied() else {
            anyhow::bail!("usage: pirs replay <session.jsonl> [--model X]");
        };
        let live_model = args
            .windows(2)
            .find(|w| w[0] == "--model")
            .map(|w| w[1].to_string());
        let tape = replay::load_cassette(std::path::Path::new(file))?;
        let cwd = std::env::current_dir()?;
        let diverged = std::sync::Arc::new(std::sync::Mutex::new(None));
        let live = live_model.is_some();

        let model = live_model.clone().unwrap_or_else(|| "replay".to_string());
        let provider: Arc<dyn pirs_ai::LlmProvider> = if live {
            if cli.provider == "anthropic" {
                Arc::new(pirs_ai::AnthropicClient::new(cli.base_url.clone()))
            } else {
                Arc::new(OpenAiCompat::new(cli.base_url.clone()))
            }
        } else {
            Arc::new(replay::ReplayProvider::new(&tape))
        };
        let tools: Vec<Arc<dyn pirs_agent::AgentTool>> = pirs_tools::default_tools(cwd.clone())
            .into_iter()
            .map(|t| {
                Arc::new(replay::CassetteTool::wrap(
                    t,
                    &tape,
                    live,
                    std::sync::Arc::clone(&diverged),
                )) as Arc<dyn pirs_agent::AgentTool>
            })
            .collect();
        // Live replay calls the real LLM, so it must see the same system
        // prompt the recording used — the default placeholder would produce a
        // different session and every message would spuriously "diverge".
        // (Strict replay drives the model from the tape, so the prompt is
        // inert there; building it unconditionally keeps one code path.)
        let mut system = system_prompt::build_system_prompt(&cwd, &tools);
        if let Some(ctx) = system_prompt::read_project_context(&cwd) {
            system.push_str(&ctx);
        }
        let mut agent = Agent::new(provider, &model)
            .with_system_prompt(system)
            .with_tools(tools);
        let produced = replay::run_replay(&mut agent, &tape).await;
        let report = replay::compare(&replay::expected_of(&tape), &produced);
        match report.divergence {
            None => {
                println!("replay: {} messages matched", report.matched);
                return Ok(());
            }
            Some(d) => {
                eprintln!(
                    "replay diverged at message {}: expected {}, got {}",
                    d.index, d.expected, d.actual
                );
                if let Some(t) = diverged.lock().unwrap().as_ref() {
                    eprintln!("first tool divergence: {t}");
                }
                std::process::exit(if live { 2 } else { 1 });
            }
        }
    }
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p.starts_with("pack install "))
    {
        // pirs pack install <git-url> [--pin <ref>] [--yes] [--force]
        let args: Vec<&str> = spec
            .trim_start_matches("pack install ")
            .split_whitespace()
            .collect();
        let Some(url) = args.first().copied() else {
            anyhow::bail!("usage: pirs pack install <git-url> [--pin <ref>] [--yes] [--force]");
        };
        let flag = |name: &str| {
            args.windows(2)
                .find(|w| w[0] == name)
                .map(|w| w[1].to_string())
        };
        let pin = flag("--pin");
        let yes = args.contains(&"--yes");
        let force = args.contains(&"--force");

        let name = pack::pack_name_from_url(url);
        eprintln!(
            "[pack: cloning {url}{}]",
            pin.as_deref()
                .map(|p| format!(" @ {p}"))
                .unwrap_or_default()
        );
        let (tmp, head) = pack::clone_pinned(url, pin.as_deref())?;
        let scripts = pack::collect_scripts(&tmp.path().join("repo"));
        if scripts.is_empty() {
            anyhow::bail!("{url}: no .rhai scripts found (root, extensions/, packs/)");
        }
        println!("pack {name} @ {head} ({} scripts):", scripts.len());
        for s in &scripts {
            let src = std::fs::read_to_string(s).unwrap_or_default();
            println!(
                "  {}: {}",
                s.file_name().and_then(|f| f.to_str()).unwrap_or("?"),
                pirs_rhai::caps::parse_caps(&src).summary()
            );
        }
        if !yes {
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                anyhow::bail!("refusing to install without confirmation (pass --yes)");
            }
            eprint!("install into ~/.pirs/packs? [y/N] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !matches!(line.trim(), "y" | "yes" | "Y") {
                anyhow::bail!("aborted");
            }
        }
        let home = std::env::var("HOME").context("HOME not set")?;
        // Installed packs go to ~/.pirs/packs, which is trust-gated (hash-bound)
        // on load — NOT ~/.pirs/extensions, which is auto-run. So remote code
        // can't execute just by landing on disk: the first run prompts (showing
        // caps) and a later tamper/pull re-prompts. --yes skips the *install*
        // confirmation only, never the load-time trust decision.
        let dest = std::path::Path::new(&home).join(".pirs").join("packs");
        let installed = pack::install_scripts(&scripts, &dest, force)?;
        for p in &installed {
            println!("installed {}", p.display());
        }
        println!(
            "note: installed packs are trust-gated; the next `pirs` run will \
             ask to trust ~/.pirs/packs before loading them."
        );
        return Ok(());
    }
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "blame" || p.starts_with("blame "))
    {
        let arg = spec.trim_start_matches("blame").trim().to_string();
        let Some((file, line)) = arg.rsplit_once(':') else {
            anyhow::bail!("usage: pirs blame <file>:<line>");
        };
        let line: u32 = line.parse().context("line must be a number")?;
        let cwd = std::env::current_dir()?;
        let info = blame::blame_line(&cwd, file, line)?;
        println!("{}", blame::format_blame(&info));
        return Ok(());
    }
    let cwd = std::env::current_dir()?;

    if cli.prompt.first().map(|s| s.as_str()) == Some("login") || cli.mode == "login" {
        let provider = if cli.provider == "anthropic" {
            "anthropic"
        } else {
            "openai"
        };
        return auth::login(provider);
    }

    // Load ~/.pirs/secrets.env into process env (does not override existing vars).
    registry::load_secrets_env();
    // Model registry first so backend api_key_env can satisfy auth without OPENAI_API_KEY.
    let model_registry = registry::load_registry_layers(&cwd);

    let env_var = if cli.provider == "anthropic" {
        "ANTHROPIC_API_KEY"
    } else {
        "OPENAI_API_KEY"
    };
    // Model-aware OpenAI-compat env fallback (DASHSCOPE/DEEPSEEK/OPENROUTER) so a
    // missing registry or empty OPENAI_API_KEY still works with secrets.env keys.
    let (compat_base, compat_key) = if cli.provider == "anthropic" {
        (None, None)
    } else {
        pirs_ai::resolve_openai_compat(Some(&cli.model))
    };
    let api_key = auth::resolve(cli.api_key.as_deref(), &cli.provider, env_var)
        .or_else(|| registry::api_key_for_alias(&model_registry, &cli.model))
        .or_else(|| {
            cli.plan_model
                .as_ref()
                .and_then(|m| registry::api_key_for_alias(&model_registry, m))
        })
        .or_else(|| registry::first_available_backend_key(&model_registry))
        .or(compat_key)
        .with_context(|| {
            let mut hint = format!(
                "no API key: pass --api-key, run `pirs login`, set {env_var}"
            );
            let mut envs = registry::expected_key_envs(&model_registry);
            for k in pirs_ai::well_known_key_envs() {
                if !envs.iter().any(|e| e == k) {
                    envs.push((*k).to_string());
                }
            }
            if !envs.is_empty() {
                hint.push_str(&format!(" (also tried {})", envs.join(" / ")));
            }
            hint.push_str(" — ensure ~/.pirs/secrets.env is loaded (HOME must point at your user home)");
            hint
        })?;

    // When the user did not pin --base-url / config base_url, use the endpoint
    // that matches the env key we resolved (deepseek vs dashscope vs …).
    if cli.base_url.is_none() {
        if let Some(b) = compat_base {
            cli.base_url = Some(b);
        }
    }

    if cli.mode == "rpc" {
        return rpc_mode::run(rpc_mode::RpcOptions {
            cwd: cwd.clone(),
            model: cli.model.clone(),
            base_url: cli.base_url.clone(),
            api_key,
            max_retries: cli.max_retries,
        })
        .await;
    }
    if cli.mode == "acp" {
        return acp_mode::run(acp_mode::AcpOptions {
            cwd: cwd.clone(),
            model: cli.model.clone(),
            base_url: cli.base_url.clone(),
            api_key,
            max_retries: cli.max_retries,
        })
        .await;
    }
    if cli.mode != "repl" && cli.mode != "tui" {
        bail!("unknown mode: {} (expected repl|rpc|tui|acp)", cli.mode);
    }

    let default_provider: Arc<dyn pirs_ai::LlmProvider> = if cli.provider == "anthropic" {
        Arc::new(
            pirs_ai::AnthropicClient::new(cli.base_url.clone()).with_max_retries(cli.max_retries),
        )
    } else if cli.provider == "openai" {
        Arc::new(
            OpenAiCompat::new(cli.base_url.clone())
                .with_max_retries(cli.max_retries)
                .with_cache_key(format!("pirs-{}-{}", std::process::id(), cli.model)),
        )
    } else {
        anyhow::bail!(
            "unknown provider '{}' (expected openai|anthropic)",
            cli.provider
        );
    };
    // Multi-backend registry: aliases in --model / --plan-model route to
    // per-subscription base_url + API key (see [[backends]] / [[models]]).
    let provider: Arc<dyn pirs_ai::LlmProvider> =
        if let Some(router) = registry::build_routing_provider(
            &model_registry,
            Arc::clone(&default_provider),
            Some(api_key.clone()),
            cli.max_retries,
        )? {
            if !model_registry.models.is_empty() {
                let aliases: Vec<_> = model_registry
                    .models
                    .iter()
                    .map(|m| m.alias.as_str())
                    .collect();
                eprintln!(
                    "[model registry: {} alias(es), {} backend(s) — {}]",
                    model_registry.models.len(),
                    model_registry.backends.len(),
                    aliases.join(", ")
                );
            }
            router
        } else {
            default_provider
        };
    let usage_slot: std::sync::Arc<std::sync::Mutex<pirs_ai::Usage>> =
        std::sync::Arc::new(std::sync::Mutex::new(pirs_ai::Usage::default()));

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let mut hooks = Hooks::default();
    let approval_mode =
        approval::ApprovalMode::parse(&cli.approval).unwrap_or(approval::ApprovalMode::Auto);
    // Plan/Act product dial maps onto profile + permission ladder.
    let mut dial_plan = false;
    if let Some(d) = cli.mode_dial.as_deref() {
        match d.trim().to_ascii_lowercase().as_str() {
            "plan" => {
                dial_plan = true;
                if cli.agent_profile == "default" {
                    cli.agent_profile = "plan".into();
                }
                if cli.permission_mode.is_none() {
                    cli.permission_mode = Some("read-only".into());
                }
                eprintln!("[mode-dial: plan — read-only tools]");
            }
            "act" => {
                if cli.permission_mode.is_none() {
                    cli.permission_mode = Some("danger-full-access".into());
                }
                eprintln!("[mode-dial: act — full tools]");
            }
            other => bail!("unknown --mode-dial {other:?}; expected plan|act"),
        }
    }
    let safety = pirs_tools::SafetyProfile::parse(&cli.agent_profile).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown --agent-profile {:?}; expected default|plan|accept-edits|auto-approve",
            cli.agent_profile
        )
    })?;
    if safety != pirs_tools::SafetyProfile::Default {
        eprintln!("[agent-profile: {}]", safety.name());
    }
    // So Rhai packs (strict-plan, etc.) can read the active profile via agent_profile("").
    std::env::set_var("PIRS_AGENT_PROFILE", safety.name());
    let perm_mode = cli
        .permission_mode
        .as_deref()
        .and_then(pirs_tools::PermissionMode::parse)
        .unwrap_or_else(pirs_tools::PermissionMode::from_env);
    std::env::set_var("PIRS_PERMISSION_MODE", perm_mode.name());
    if perm_mode != pirs_tools::PermissionMode::WorkspaceWrite || dial_plan {
        eprintln!("[permission-mode: {}]", perm_mode.name());
    }
    // Always install gate when a non-default safety profile is set (hard denials),
    // or when approval is Ask. Auto+default stays open; yolo still skips rhai policy.
    let gate = std::sync::Arc::new(approval::ApprovalGate::with_profile(
        approval_mode,
        cwd.clone(),
        safety,
    ));
    let mut gate_hook = if approval_mode == approval::ApprovalMode::Ask
        || safety != pirs_tools::SafetyProfile::Default
    {
        Some(gate.hook())
    } else {
        None
    };
    // Chain permission ladder (always on when not danger-full-access).
    if perm_mode != pirs_tools::PermissionMode::DangerFullAccess {
        let ph = pirs_tools::permission_hook(perm_mode);
        gate_hook = pirs_agent::Hooks::chain_before(gate_hook, Some(ph));
    }
    let _ = dial_plan;

    // Semantic search needs the vector store, so it implies the persistent graph.
    let graph_db = cwd.join(".pirs").join("graph.db");
    let graph: Option<std::sync::Arc<pirs_graph::LazyGraph>> = if cli.no_graph {
        None
    } else if cli.persist_graph || cli.semantic {
        Some(std::sync::Arc::new(pirs_graph::LazyGraph::persistent(
            cwd.clone(),
            graph_db.clone(),
        )))
    } else {
        Some(std::sync::Arc::new(pirs_graph::LazyGraph::new(cwd.clone())))
    };
    let mut sub_tools = tools.clone();
    if let Some(g) = &graph {
        let map_tool = std::sync::Arc::new(pirs_graph::code_map::CodeMapTool::new(
            std::sync::Arc::clone(g),
            cwd.clone(),
        ));
        let ast_tool = std::sync::Arc::new(pirs_graph::ast_edit::AstEditTool::new(cwd.clone()));
        tools.push(map_tool.clone());
        tools.push(ast_tool.clone());
        sub_tools.push(map_tool);
        sub_tools.push(ast_tool);

        // The optional semantic arm of code_search. BM25 + graph work with no
        // model; embeddings are added only when --semantic supplies one.
        let embedder = if cli.semantic {
            match cli.embed_model.clone() {
                Some(model) => {
                    let base = cli
                        .embed_base_url
                        .clone()
                        .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
                    Some(pirs_ai::EmbeddingClient::new(
                        base,
                        model,
                        cli.embed_api_key.clone(),
                    ))
                }
                None => {
                    eprintln!(
                        "warning: --semantic requires --embed-model (or PIRS_EMBED_MODEL); \
                         code_search will run lexical+graph only"
                    );
                    None
                }
            }
        } else {
            None
        };
        // Indexing strategy:
        //  - default: a background indexer fills the embedding index in
        //    checkpointed batches, and code_search is query-only (embed_cap = 0)
        //    so a search NEVER blocks on embedding — BM25 answers instantly and
        //    semantic hits light up as vectors land.
        //  - --embed-batch-cap N: opt into synchronous inline indexing instead
        //    (code_search embeds up to N symbols per call, no background task) —
        //    useful for a one-shot where you want the index built in-process.
        let inline = cli.embed_batch_cap.is_some();
        let code_cap = if embedder.is_some() {
            if inline {
                cli.embed_batch_cap
            } else {
                Some(0)
            }
        } else {
            None
        };
        if let (Some(emb), false) = (&embedder, inline) {
            let bg = pirs_graph::BackgroundIndexer::new(
                cwd.clone(),
                graph_db.clone(),
                emb.clone(),
                cli.embed_max_chars.unwrap_or(2000),
            );
            tokio::spawn(bg.run());
        }
        // Reused for `recall`'s semantic mode below — cloned before the move
        // into CodeSearchTool::new, since both tools want the same embedder
        // rather than each constructing (and each paying for) their own.
        let recall_embedder = embedder.clone();
        let code_search = std::sync::Arc::new(pirs_graph::code_search::CodeSearchTool::new(
            std::sync::Arc::clone(g),
            cwd.clone(),
            graph_db.clone(),
            embedder,
            cli.embed_max_chars,
            code_cap,
        ));
        tools.push(code_search.clone());
        sub_tools.push(code_search);

        // Upgrades the bare `recall` tool already in `tools` (from
        // default_tools()) to one that also supports `mode: "semantic"` —
        // the later-registered tool with the same name wins on dispatch, so
        // this doesn't need to remove the earlier one.
        if let Some(emb) = recall_embedder {
            let recall = std::sync::Arc::new(pirs_tools::RecallTool::with_embedder(emb));
            tools.push(recall.clone());
            sub_tools.push(recall);
        }
    }
    {
        let manifests = [
            "Cargo.toml",
            "package.json",
            "go.mod",
            "pyproject.toml",
            "setup.py",
        ];
        let has_project = manifests.iter().any(|m| cwd.join(m).exists());
        let has_server = pirs_lsp::client::SERVERS
            .iter()
            .any(pirs_lsp::client::server_available);
        if has_project && has_server {
            let found: Vec<&str> = pirs_lsp::client::SERVERS
                .iter()
                .filter(|s| pirs_lsp::client::server_available(s))
                .map(|s| s.language)
                .collect();
            eprintln!("[lsp: {}]", found.join(", "));
            let lsp_tool = std::sync::Arc::new(pirs_lsp::tool::LspTool::new(cwd.clone()));
            tools.push(lsp_tool.clone());
            sub_tools.push(lsp_tool);
            // Compound rename: one call rewrites a symbol across the project via
            // the language server's own reference analysis.
            let rename_tool =
                std::sync::Arc::new(pirs_lsp::rename::RenameSymbolTool::new(cwd.clone()));
            tools.push(rename_tool.clone());
            sub_tools.push(rename_tool);
        }
    }
    let mut policy_hooks: Option<(
        pirs_agent::events::BeforeToolCallHook,
        pirs_agent::events::AfterToolCallHook,
    )> = None;
    let policy_slot: std::sync::Arc<
        std::sync::Mutex<
            Option<(
                pirs_agent::events::BeforeToolCallHook,
                pirs_agent::events::AfterToolCallHook,
            )>,
        >,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));

    let host = if cli.no_extensions {
        None
    } else {
        pirs_rhai::register_core_host_apis();
        let mut h = pirs_rhai::ExtensionHost::new();
        if let Some(g) = &graph {
            let g = std::sync::Arc::clone(g);
            let cwd_q = cwd.clone();
            pirs_rhai::register_query_fn("graph_affected_tests", move |path| {
                let p = std::path::PathBuf::from(path);
                let abs = if p.is_absolute() { p } else { cwd_q.join(p) };
                g.get().affected_tests(&abs)
            });
        }
        h.set_subagent_runner(subagent::build_subagent_runner(
            std::sync::Arc::clone(&provider),
            CompletionOptions {
                api_key: Some(api_key.clone()),
                ..Default::default()
            },
            cli.model.clone(),
            sub_tools.clone(),
            std::sync::Arc::clone(&policy_slot),
            std::sync::Arc::clone(&usage_slot),
        ));
        // Bundled weak packs load first; project/user extensions load after so
        // a local weak-model.rhai (or other pack) overrides by last-wins.
        if cli.weak {
            pirs_rhai::weak_packs::load_into(&mut h);
        }
        h.load_default_dirs(&cwd);
        for err in &h.load_errors {
            eprintln!("[extension error] {err}");
        }
        let h = Arc::new(h);
        if !h.extension_names().is_empty() {
            eprintln!("[extensions: {}]", h.extension_names().join(", "));
        }
        tools.extend(h.tools());
        let ext_hooks = h.hooks();
        let yolo =
            approval::ApprovalMode::parse(&cli.approval) == Some(approval::ApprovalMode::Yolo);
        // Subagents inherit gate+extension policy. Previously required BOTH
        // before and after hooks, so packs with only on_tool_call (strict-plan,
        // session-discipline, weak-model) never reached subagents.
        let chained_before = chain_gate_with_extensions(
            gate_hook.clone(),
            ext_hooks.before_tool_call.clone(),
            yolo,
            safety,
        );
        let after_for_sub = ext_hooks.after_tool_call.clone().unwrap_or_else(|| {
            std::sync::Arc::new(|_id, _name, _result| None)
        });
        if chained_before.is_some() || ext_hooks.after_tool_call.is_some() {
            let b = chained_before.unwrap_or_else(|| {
                std::sync::Arc::new(|_id, _name, _args| None)
            });
            *policy_slot.lock().unwrap() = Some((b, after_for_sub));
            policy_hooks = policy_slot.lock().unwrap().clone();
        }
        // Extension before/after hooks always install (weak-model loop detection,
        // verify-after-edit tracking). YOLO skips interactive approval prompts
        // but still chains `--agent-profile` hard denials when profile != default.
        hooks.before_tool_call = chain_gate_with_extensions(
            gate_hook.clone(),
            ext_hooks.before_tool_call,
            yolo,
            safety,
        );
        {
            let rhai_after = ext_hooks.after_tool_call;
            let graph_after = graph.clone().map(|g| {
                let g = std::sync::Arc::clone(&g);
                let g = std::sync::Arc::clone(&g);
                let cwd2 = cwd.clone();
                let f: pirs_agent::events::AfterToolCallHook =
                    std::sync::Arc::new(move |_id, name, result| {
                        if (name != "edit"
                            && name != "edit_block"
                            && name != "write"
                            && name != "ast_edit")
                            || result.is_error
                        {
                            return None;
                        }
                        let path = result
                            .details
                            .as_ref()
                            .and_then(|d| d.get("path"))
                            .and_then(|p| p.as_str())
                            .map(std::path::PathBuf::from)
                            .or_else(|| {
                                result
                                    .content
                                    .iter()
                                    .filter_map(|b| b.as_text())
                                    .next()
                                    .and_then(|t| {
                                        t.rsplit_once(" in ")
                                            .map(|(_, p)| std::path::PathBuf::from(p.trim()))
                                    })
                            })?;
                        let abs = if path.is_absolute() {
                            path
                        } else {
                            cwd2.join(path)
                        };
                        let graph = g.get();
                        let mut notes: Vec<String> = Vec::new();
                        for sym in graph.file_symbols(&abs) {
                            let n = graph.callers(&sym.name).len();
                            if n > 0 {
                                notes.push(format!(
                                    "{} ({} caller{})",
                                    sym.name,
                                    n,
                                    if n == 1 { "" } else { "s" }
                                ));
                            }
                        }
                        // Mark the index stale on EVERY edit — before any early
                        // return — so the next graph query rebuilds against the
                        // edited tree. (Previously this ran only when notes were
                        // non-empty and was skipped entirely when a rhai after-
                        // hook returned Some, freezing the graph.)
                        g.invalidate();
                        if notes.is_empty() {
                            return None;
                        }
                        let mut content = result.content.clone();
                        let total_callers: usize = graph
                            .file_symbols(&abs)
                            .iter()
                            .map(|s| graph.callers(&s.name).len())
                            .sum();
                        content.push(pirs_ai::ContentBlock::text(format!(
                            "Blast radius: {} graph caller(s) of edited symbols: {}",
                            total_callers,
                            notes.join(", ")
                        )));
                        Some(pirs_agent::ToolResultPatch {
                            content: Some(content),
                            ..Default::default()
                        })
                    });
                f
            });
            hooks.after_tool_call = match (rhai_after, graph_after) {
                // Always run the graph hook (for its invalidation side-effect),
                // then prefer the rhai patch, falling back to the blast-radius
                // note. Running graph_after unconditionally is what keeps the
                // index fresh when an extension's after-hook returns Some.
                (Some(r), Some(g)) => Some(std::sync::Arc::new(move |id, name, result| {
                    let graph_patch = g(id, name, result);
                    r(id, name, result).or(graph_patch)
                })),
                (a, b) => a.or(b),
            };
        }
        hooks.transform_context = ext_hooks.transform_context;
        hooks.should_stop_after_turn = ext_hooks.should_stop_after_turn;
        hooks.get_steering_messages = ext_hooks.get_steering_messages;
        hooks.get_follow_up_messages = ext_hooks.get_follow_up_messages;
        Some(h)
    };

    // The approval gate must be installed even with --no-extensions: the
    // chained install above only runs in the extensions branch, and without
    // this fallback `--approval ask --no-extensions` had no gate at all.
    install_gate_if_absent(&mut hooks, &gate_hook, &cli.approval);
    // yolo + --agent-profile plan (etc.) with no extensions: still enforce denials.
    install_profile_under_yolo_if_needed(&mut hooks, &gate_hook, &cli.approval, safety);

    // Subagents must inherit profile/approval even when --no-extensions left
    // policy_slot empty (previously only filled inside the extensions branch).
    {
        let yolo =
            approval::ApprovalMode::parse(&cli.approval) == Some(approval::ApprovalMode::Yolo);
        if policy_slot.lock().unwrap().is_none() {
            if let Some(b) =
                chain_gate_with_extensions(gate_hook.clone(), None, yolo, safety)
            {
                *policy_slot.lock().unwrap() = Some((
                    b,
                    std::sync::Arc::new(|_id, _name, _result| None),
                ));
            }
        }
    }

    if !cli.no_mcp {
        let mcp = pirs_mcp::load_servers(&cwd).await;
        for err in &mcp.errors {
            eprintln!("[mcp error] {err}");
        }
        if !mcp.handles.is_empty() {
            let names: Vec<String> = mcp.handles.iter().map(|h| h.name.clone()).collect();
            eprintln!("[mcp: {} ({} tools)]", names.join(", "), mcp.tools.len());
        }
        let rep = pirs_mcp::McpDegradedReport::from_load(&mcp);
        if !rep.working.is_empty() || !rep.failed.is_empty() {
            std::env::set_var("PIRS_MCP_DOCTOR_LINES", rep.lines().join("\n"));
        }
        tools.extend(mcp.tools);
    }

    let skills = pirs_skills::discover_skills(&cwd);
    let file_commands = discovery::discover_commands(&cwd);
    // Shared skill tools (same crate as pirs-claw).
    tools.extend(pirs_skills::skill_tools(
        std::sync::Arc::new(skills.clone()),
        true,
    ));

    // Inject a PageRank-ranked symbol sketch so the model sees structure
    // without a first tool call (classic repomap idea). Weak mode gets a
    // larger budget (weaker models thrash without a map).
    let repo_map = if cli.no_repo_map {
        None
    } else {
        graph.as_ref().and_then(|g| {
            let budget = if cli.weak {
                6_000
            } else {
                pirs_graph::repo_map::DEFAULT_MAP_CHARS
            };
            pirs_graph::repo_map::render_sketch(&g.get(), &cwd, budget)
        })
    };
    if let Some(ref m) = repo_map {
        eprintln!("[repo_map: {} chars]", m.len());
    }
    let mut system =
        system_prompt::build_system_prompt_with_map(&cwd, &tools, repo_map.as_deref(), cli.weak);
    // Progressive agentskills index (shared with pirs-claw).
    system.push_str(&pirs_skills::skills_prompt_section(&skills));
    if let Some(h) = &host {
        let cmds = h.commands();
        if !cmds.is_empty() {
            system.push_str("\nCustom commands (from extensions):\n");
            for (name, desc) in &cmds {
                system.push_str(&format!("- /{name}: {desc}\n"));
            }
        }
    }
    if let Some(ctx) = system_prompt::read_project_context(&cwd) {
        system.push_str(&ctx);
    }

    let completion = CompletionOptions {
        api_key: Some(api_key.clone()),
        ..Default::default()
    };

    let compaction = if cli.no_compaction {
        None
    } else {
        Some(pirs_agent::compaction::CompactionConfig {
            context_window: cli.context_window,
            ..Default::default()
        })
    };

    {
        let delegate_provider: std::sync::Arc<dyn pirs_ai::LlmProvider> = if cli.provider
            == "anthropic"
        {
            std::sync::Arc::new(
                pirs_ai::AnthropicClient::new(cli.base_url.clone())
                    .with_max_retries(cli.max_retries),
            )
        } else {
            std::sync::Arc::new(
                pirs_ai::OpenAiCompat::new(cli.base_url.clone()).with_max_retries(cli.max_retries),
            )
        };
        let delegate_completion = CompletionOptions {
            api_key: Some(api_key.clone()),
            ..Default::default()
        };
        let delegate_model = cli.model.clone();
        let _delegate_cwd = cwd.clone();
        let delegate_tools = sub_tools.clone();
        let delegate = pirs_agent::delegate::DelegateTool::new(
            delegate_provider,
            delegate_model,
            delegate_completion,
            move || delegate_tools.clone(),
        );
        if let Some((b, a)) = &policy_hooks {
            delegate.with_policy_hooks(b.clone(), a.clone());
        }
        tools.push(delegate);
    }

    let (visible, mut tools) = if cli.tool_diet {
        let set: pirs_agent::agent_loop::VisibleTools = std::sync::Arc::new(std::sync::Mutex::new(
            pirs_agent::use_tool::UseTool::default_visible(),
        ));
        let use_tool = pirs_agent::use_tool::UseTool::new(&set, &tools);
        tools.push(use_tool);
        (Some(set), tools)
    } else {
        (None, tools)
    };
    let _ = &mut tools;

    let execution = if cli.sequential {
        pirs_agent::ExecutionMode::Sequential
    } else {
        pirs_agent::ExecutionMode::Parallel
    };

    let approval_mode = approval::ApprovalMode::parse(&cli.approval).unwrap_or_else(|| {
        eprintln!("[unknown approval mode '{}', using auto]", cli.approval);
        approval::ApprovalMode::Auto
    });
    if approval_mode == approval::ApprovalMode::Yolo {
        eprintln!("[WARNING: yolo mode — no approvals, no policy hooks. All tool calls execute.]");
    }

    let cascade_cfg =
        cli.cascade
            .as_ref()
            .map(|draft_model| pirs_agent::agent_loop::CascadeConfig {
                draft_model: draft_model.clone(),
                judge: subagent::build_cascade_judge(
                    std::sync::Arc::clone(&provider),
                    draft_model.clone(),
                ),
            });

    // Strategy/profile mode needs the full tool list after the agent takes it, to
    // re-scope tools per phase. Clone the Arc handles up front (cheap) only then.
    let strategy_mode = cli.strategy.is_some() || cli.profile.is_some();
    // TUI can enable strategy mid-session, so keep a full tool clone whenever
    // we might run phases (strategy/profile mode or TUI).
    let strategy_tools: Vec<Arc<dyn AgentTool>> =
        if strategy_mode || cli.mode == "tui" {
            tools.clone()
        } else {
            Vec::new()
        };

    let mut agent = Agent::new(provider, &cli.model)
        .with_system_prompt(system)
        .with_tools(tools)
        .with_completion(completion)
        .with_hooks(hooks)
        .with_compaction(compaction)
        .with_visible_tools(visible)
        .with_tool_execution(execution)
        .with_cascade(cascade_cfg)
        .with_budgets(pirs_agent::agent_loop::Budgets {
            max_turns: cli.max_turns,
            max_tool_calls: cli.max_tool_calls,
            max_wall_time: cli.max_wall_time.map(std::time::Duration::from_secs),
        });
    agent.set_extra_usage_handle(usage_slot.clone());
    {
        let steer = agent.steer_sender();
        pirs_agent::jobs::registry().set_notifier(std::sync::Arc::new(move |msg| {
            steer(Message::user(msg));
        }));
    }
    let approval_shared = gate.shared_mode();

    let session_path = session::session_path(&cwd)?;
    let session_stem = session_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    pirs_rhai::set_session_meta(&session_stem, &cli.model);

    // First-class action audit (not pack-only). Disable with PIRS_AUDIT=0.
    {
        let audit = pirs_agent::AuditLog::default_open();
        if pirs_agent::audit_enabled() {
            eprintln!("[audit: {}]", audit.path().display());
        }
        agent.subscribe(pirs_agent::audit_listener(audit));
    }

    // Optional flight recorder: agent events + strategy phase boundaries.
    let run_id = observability::make_run_id(&session_stem);
    let trace_path = observability::resolve_trace_path(cli.trace.as_deref(), &run_id);
    let trace_phase: Arc<Mutex<String>> = Arc::new(Mutex::new("main".into()));
    let recorder: Option<Arc<pirs_agent::trace::Recorder>> = match &trace_path {
        Some(path) => {
            let rec = observability::open_recorder(path, &run_id)?;
            let aliases: Vec<String> = model_registry
                .models
                .iter()
                .map(|m| m.alias.clone())
                .collect();
            observability::record_run_config(
                &rec,
                &cli.model,
                cli.plan_model.as_deref(),
                cli.strategy.as_deref().or(cli.profile.as_deref()),
                &aliases,
            );
            observability::attach_agent_trace(
                &mut agent,
                Arc::clone(&rec),
                Arc::clone(&trace_phase),
            );
            Some(rec)
        }
        None => None,
    };
    if let Err(e) = pirs_agent::memory::init_global(&cwd.join(".pirs").join("memory.db")) {
        eprintln!("[memory disabled: {e}]");
    } else {
        // Scope recall to this session so it doesn't surface stale hits from
        // unrelated past tasks in the same repo.
        pirs_agent::memory::set_session(
            &session_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        );
    }
    if cli.resume {
        match session::load_latest(&cwd) {
            Ok((path, messages)) => {
                eprintln!("[resumed {} ({} messages)]", path.display(), messages.len());
                let n = messages.len();
                agent.messages = messages;
                session::append(&session_path, &agent.messages)?;
                eprintln!("[carried {} messages into the new session file]", n);
            }
            Err(e) => eprintln!("[no session to resume: {e}]"),
        }
    }

    let printer = Arc::new(Printer::new());
    let session_file_shared = std::sync::Arc::new(std::sync::Mutex::new(session_path.clone()));
    {
        let sf = std::sync::Arc::clone(&session_file_shared);
        agent.subscribe(Arc::new(move |event: AgentEvent| {
            if let AgentEvent::MessageEnd { message } = event {
                let path = sf.lock().unwrap().clone();
                let _ = session::append(&path, &[*message]);
            }
        }));
    }
    if let Some(h) = &host {
        if let Some(l) = h.listener() {
            agent.subscribe(l);
        }
    }
    let printed = Arc::new(Mutex::new((0usize, 0usize)));
    if cli.mode == "repl" {
        let p = Arc::clone(&printer);
        let printed = Arc::clone(&printed);
        agent.subscribe(Arc::new(move |event: AgentEvent| match &event {
            AgentEvent::MessageStart { message } if message.is_assistant() => {
                *printed.lock().unwrap() = (0, 0);
            }
            AgentEvent::MessageUpdate { message } => {
                let text = message.text();
                let thinking: String = message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        pirs_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                let mut n = printed.lock().unwrap();
                if thinking.len() > n.1 {
                    print!("\x1b[2;3m{}\x1b[0m", &thinking[n.1..]);
                    let _ = std::io::stdout().flush();
                    n.1 = thinking.len();
                }
                if text.len() > n.0 {
                    print!("{}", &text[n.0..]);
                    let _ = std::io::stdout().flush();
                    n.0 = text.len();
                }
            }
            _ => p.event(event),
        }));
    }

    if cli.serve {
        let loopback = matches!(cli.bind.as_str(), "127.0.0.1" | "localhost" | "::1");
        let token = match cli.serve_token.clone() {
            Some(t) => t,
            None => {
                if !loopback {
                    anyhow::bail!(
                        "--serve-token (or PIRS_SERVE_TOKEN) is required for a non-loopback bind ({})",
                        cli.bind
                    );
                }
                generate_serve_token()
            }
        };
        eprintln!("[serve token: {token}]");
        return serve::run(serve::ServeOptions {
            agent,
            host,
            port: cli.port,
            bind: cli.bind.clone(),
            token,
            allow_external: cli.serve_external,
        })
        .await;
    }

    if cli.mode == "tui" {
        let aliases: Vec<String> = model_registry
            .models
            .iter()
            .map(|m| m.alias.clone())
            .collect();
        return tui::run(tui::TuiOptions {
            agent,
            host,
            session_path,
            approval_mode,
            approval_gate: Some(gate),
            cwd,
            strategy: cli.strategy.clone(),
            plan_model: cli.plan_model.clone(),
            verify: cli.verify.clone(),
            max_attempts: cli.max_attempts,
            strategy_tools,
            recorder: recorder.clone(),
            trace_phase: Some(Arc::clone(&trace_phase)),
            model_aliases: aliases,
        })
        .await;
    }

    if let Some(prompt) = cli.prompt.first().cloned() {
        if strategy_mode {
            let (report, passed) = run_strategy_turn(
                &agent,
                &prompt,
                cli.strategy.as_deref(),
                cli.profile.as_deref(),
                &cli.model,
                cli.plan_model.as_deref(),
                strategy_tools,
                &cwd,
                cli.verify.as_deref(),
                cli.max_attempts,
                recorder.as_ref(),
                Some(Arc::clone(&trace_phase)),
            )
            .await?;
            eprintln!();
            print_usage(&report);
            // A --verify gate (including weak auto-verify) that never passed
            // exits non-zero so scripts/CI can tell a green run from a red one.
            if cli.verify.is_some() && !passed {
                std::process::exit(1);
            }
            return Ok(());
        }
        run_turn(
            &mut agent,
            &prompt,
            &printer,
            &session_path,
            approval_mode,
            host.as_ref(),
        )
        .await?;
        // Shared learning loop (same as pirs-claw): crystallize after substantial one-shots.
        if pirs_skills::learn_enabled_cli() {
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
            let transcript =
                pirs_skills::session_transcript(&prompt, &reply, "pirs one-shot");
            let _ = pirs_skills::maybe_crystallize_skill(
                agent.provider.clone(),
                &agent.model,
                Some(api_key.clone()),
                &transcript,
                400,
            )
            .await;
        }
        eprintln!();
        print_usage(&agent.usage_report());
        if let Some(hit) = agent.budget_hit {
            eprintln!("[budget exhausted: {hit:?}]");
            std::process::exit(match hit {
                pirs_agent::agent_loop::BudgetHit::Turns => 53,
                pirs_agent::agent_loop::BudgetHit::WallTime => 54,
                pirs_agent::agent_loop::BudgetHit::ToolCalls => 55,
            });
        }
        return Ok(());
    }

    repl(
        &mut agent,
        &printer,
        &session_file_shared,
        &cwd,
        host.as_ref(),
        &file_commands,
        approval_shared,
    )
    .await
}

async fn run_turn(
    agent: &mut Agent,
    input: &str,
    _printer: &Arc<Printer>,
    _session_path: &Path,
    approval_mode: approval::ApprovalMode,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
) -> anyhow::Result<()> {
    let cancel = agent.cancel_handle();
    let steer_handle = if approval_mode == approval::ApprovalMode::Ask {
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
async fn run_verify_command(cmd: String, cwd: PathBuf) -> (bool, String) {
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
pub(crate) async fn run_strategy_turn(
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
                    observability::record_phase_start(rec, req);
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

struct SteerHandle {
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

async fn repl(
    agent: &mut Agent,
    printer: &Arc<Printer>,
    session_path: &std::sync::Arc<std::sync::Mutex<PathBuf>>,
    cwd: &Path,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
    file_commands: &[discovery::FileCommand],
    approval_shared: std::sync::Arc<std::sync::Mutex<approval::ApprovalMode>>,
) -> anyhow::Result<()> {
    let mut rl = DefaultEditor::new()?;
    let mut clock = session_stats::SessionClock::new();
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
                if let Err(e) = run_turn(agent, line, printer, &sp, mode, host).await {
                    eprintln!("[error: {e}]");
                }
                clock.agent_end();
                clock.absorb_messages(&agent.messages[before..]);
                // Long-term memory of the user (soul + memory.db) when durable.
                if pirs_skills::learn_enabled_interactive() || pirs_skills::looks_durable(&user_line)
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
                        .and_then(|p| {
                            p.file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                        })
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
    session_stats::print_session_stats(
        &clock,
        &agent.usage_report(),
        &agent.model,
        None,
        None,
    );
    Ok(())
}

async fn run_local_bash(cmd: &str, cwd: &Path, record: bool, agent: &mut Agent) {
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

async fn handle_command(
    line: &str,
    agent: &mut Agent,
    session_path: &std::sync::Arc<std::sync::Mutex<PathBuf>>,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
    file_commands: &[discovery::FileCommand],
    printer: &Arc<Printer>,
    approval_shared: &std::sync::Arc<std::sync::Mutex<approval::ApprovalMode>>,
    clock: &mut session_stats::SessionClock,
) -> anyhow::Result<bool> {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "/quit" | "/exit" => return Ok(true),
        "/help" => {
            for fc in file_commands {
                println!("/{:<12} {}", fc.name, fc.description);
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
                 /permission [m] read-only|workspace-write|danger-full-access\n\
                 /checkpoint     list|create|restore [id]\n\
                 /approval       auto|ask|yolo\n\
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
                "read-only"
            } else {
                "danger-full-access"
            };
            std::env::set_var("PIRS_PERMISSION_MODE", mode);
            if cmd == "/plan" {
                std::env::set_var("PIRS_AGENT_PROFILE", "plan");
            }
            println!(
                "mode → {} (permission={}; new denials apply on next tool call)",
                cmd.trim_start_matches('/'),
                mode
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
                "create" => match pirs_tools::create_checkpoint(&cwd, "manual", agent.messages.len())
                {
                    Ok(m) => println!("created {}", m.id),
                    Err(e) => eprintln!("[checkpoint] {e}"),
                },
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
                    std::env::var("PIRS_PERMISSION_MODE")
                        .unwrap_or_else(|_| "workspace-write".into())
                );
            } else if pirs_tools::PermissionMode::parse(arg).is_some() {
                std::env::set_var("PIRS_PERMISSION_MODE", arg);
                println!("permission-mode → {arg}");
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
            session_stats::print_session_stats(
                clock,
                &agent.usage_report(),
                &agent.model,
                None,
                None,
            );
        }
        "/approval" => {
            if arg.is_empty() {
                println!("approval mode: {}", approval_shared.lock().unwrap().name());
            } else if let Some(m) = approval::ApprovalMode::parse(arg) {
                *approval_shared.lock().unwrap() = m;
                println!("approval mode set to {}", m.name());
            } else {
                println!("usage: /approval <auto|ask|yolo>");
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
                session::fork_session(&session_path.lock().unwrap().clone(), idx)?;
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
                session::lineage(&session_path.lock().unwrap().clone())
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
                    let prompt = discovery::expand_command(fc, arg);
                    let mode = *approval_shared.lock().unwrap();
                    let sp = session_path.lock().unwrap().clone();
                    if let Err(e) = run_turn(agent, &prompt, printer, &sp, mode, host).await {
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
fn attach_image_message(agent: &mut Agent, path: &Path) -> anyhow::Result<String> {
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

/// A 256-bit random bearer token, hex-encoded. Never derive serve auth from a
/// clock: a timestamp token is brute-forceable from the process start time.
fn generate_serve_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("getrandom failed to produce a serve token");
    let mut s = String::with_capacity(64);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_token_is_random_and_long() {
        let a = generate_serve_token();
        let b = generate_serve_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must not be predictable/repeating");
    }
    use std::sync::Arc;

    fn gate() -> Option<pirs_agent::events::BeforeToolCallHook> {
        Some(Arc::new(|_, name, _| {
            if name == "danger" {
                Some("blocked by gate".to_string())
            } else {
                None
            }
        }))
    }

    #[test]
    fn gate_installed_when_hooks_empty() {
        // --approval ask --no-extensions: previously no gate was installed.
        let mut hooks = pirs_agent::Hooks::default();
        install_gate_if_absent(&mut hooks, &gate(), "ask");
        let before = hooks.before_tool_call.expect("gate must be installed");
        assert_eq!(
            before("1", "danger", &serde_json::json!({})).as_deref(),
            Some("blocked by gate")
        );
    }

    #[test]
    fn gate_not_installed_in_yolo() {
        let mut hooks = pirs_agent::Hooks::default();
        install_gate_if_absent(&mut hooks, &gate(), "yolo");
        assert!(hooks.before_tool_call.is_none());
    }

    #[test]
    fn existing_hook_not_overwritten() {
        let mut hooks = pirs_agent::Hooks {
            before_tool_call: Some(Arc::new(|_, _, _| Some("ext".to_string()))),
            ..Default::default()
        };
        install_gate_if_absent(&mut hooks, &gate(), "ask");
        let before = hooks.before_tool_call.unwrap();
        assert_eq!(
            before("1", "x", &serde_json::json!({})).as_deref(),
            Some("ext")
        );
    }

    #[test]
    fn yolo_with_plan_profile_installs_denials_without_extensions() {
        let mut hooks = pirs_agent::Hooks::default();
        install_gate_if_absent(&mut hooks, &gate(), "yolo");
        assert!(hooks.before_tool_call.is_none());
        install_profile_under_yolo_if_needed(
            &mut hooks,
            &gate(),
            "yolo",
            pirs_tools::SafetyProfile::Plan,
        );
        let before = hooks.before_tool_call.expect("profile under yolo");
        assert_eq!(
            before("1", "danger", &serde_json::json!({})).as_deref(),
            Some("blocked by gate")
        );
    }

    #[test]
    fn yolo_with_plan_chains_gate_before_extension() {
        let ext: pirs_agent::events::BeforeToolCallHook =
            Arc::new(|_, _, _| Some("ext-deny".into()));
        let chained = chain_gate_with_extensions(
            gate(),
            Some(ext),
            true,
            pirs_tools::SafetyProfile::Plan,
        )
        .expect("chained");
        // Gate runs first: danger blocked by gate, not ext.
        assert_eq!(
            chained("1", "danger", &serde_json::json!({})).as_deref(),
            Some("blocked by gate")
        );
        // Non-danger falls through to extension.
        assert_eq!(
            chained("1", "read", &serde_json::json!({})).as_deref(),
            Some("ext-deny")
        );
    }

    #[test]
    fn pure_yolo_with_default_profile_keeps_extension_only() {
        let ext: pirs_agent::events::BeforeToolCallHook =
            Arc::new(|_, _, _| Some("ext-only".into()));
        let chained = chain_gate_with_extensions(
            gate(),
            Some(ext),
            true,
            pirs_tools::SafetyProfile::Default,
        )
        .expect("ext only");
        // Gate must not run under pure yolo.
        assert_eq!(
            chained("1", "danger", &serde_json::json!({})).as_deref(),
            Some("ext-only")
        );
    }

    #[test]
    fn chain_with_before_only_ext_still_returns_gate_under_plan() {
        // Packs like strict-plan only register on_tool_call (before), no after.
        let ext: pirs_agent::events::BeforeToolCallHook =
            Arc::new(|_, name, _| {
                if name == "web_search" {
                    Some("strict".into())
                } else {
                    None
                }
            });
        let chained = chain_gate_with_extensions(
            gate(),
            Some(ext),
            false,
            pirs_tools::SafetyProfile::Plan,
        )
        .expect("before-only chain");
        assert_eq!(
            chained("1", "danger", &serde_json::json!({})).as_deref(),
            Some("blocked by gate")
        );
        assert_eq!(
            chained("1", "web_search", &serde_json::json!({})).as_deref(),
            Some("strict")
        );
    }
}
