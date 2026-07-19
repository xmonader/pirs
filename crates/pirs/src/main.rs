use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use clap::Parser;
use pirs_agent::{Agent, AgentEvent, AgentTool, Hooks};
use pirs_ai::{CompletionOptions, Message, OpenAiCompat};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

mod approval;
mod auth;
mod blame;
mod discovery;
mod pack;
mod replay;
mod rpc_mode;
mod serve;
mod session;
mod subagent;
mod system_prompt;
mod tui;

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

    /// Run mode: interactive REPL or headless JSONL-over-stdio RPC
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

    /// Disable rhai extension loading
    #[arg(long)]
    no_extensions: bool,

    /// Disable MCP server connections (.mcp.json)
    #[arg(long)]
    no_mcp: bool,

    /// Approval mode: auto (no prompts), ask (inline y/n for sensitive ops), yolo (no prompts, no policy hooks — dangerous)
    #[arg(long, env = "PIRS_APPROVAL", default_value = "auto")]
    approval: String,

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

    /// Start with only core tools loaded; model loads more via use_tool
    #[arg(long)]
    tool_diet: bool,

    /// Execute tool calls one at a time (helps weaker models)
    #[arg(long)]
    sequential: bool,

    /// Draft each turn with a cheaper model; escalate to the main model only when the draft is rejected
    #[arg(long)]
    cascade: Option<String>,

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
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("\n");
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

/// Installs the approval gate as the before-tool hook when nothing else
/// claimed the slot. Yolo mode explicitly waives the gate.
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
    for (model, u) in &report.by_model {
        eprintln!(
            "  {model}: input {} (cached {}) output {} total {}",
            u.input, u.cache_read, u.output, u.total_tokens
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

    let mut cli = Cli::parse();
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
        let tools: Vec<Arc<dyn pirs_agent::AgentTool>> = pirs_tools::default_tools(cwd)
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
        let mut agent = Agent::new(provider, &model).with_tools(tools);
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

    let env_var = if cli.provider == "anthropic" {
        "ANTHROPIC_API_KEY"
    } else {
        "OPENAI_API_KEY"
    };
    let api_key =
        auth::resolve(cli.api_key.as_deref(), &cli.provider, env_var).with_context(|| {
            format!("no API key: pass --api-key, run `pirs login`, or set {env_var}")
        })?;

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
    if cli.mode != "repl" && cli.mode != "tui" {
        bail!("unknown mode: {} (expected repl|rpc|tui)", cli.mode);
    }

    let provider: Arc<dyn pirs_ai::LlmProvider> = if cli.provider == "anthropic" {
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
    let usage_slot: std::sync::Arc<std::sync::Mutex<pirs_ai::Usage>> =
        std::sync::Arc::new(std::sync::Mutex::new(pirs_ai::Usage::default()));

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let mut hooks = Hooks::default();
    let approval_mode =
        approval::ApprovalMode::parse(&cli.approval).unwrap_or(approval::ApprovalMode::Auto);
    let gate = std::sync::Arc::new(approval::ApprovalGate::new(approval_mode, cwd.clone()));
    let gate_hook = if approval_mode == approval::ApprovalMode::Ask {
        Some(gate.hook())
    } else {
        None
    };

    let graph: Option<std::sync::Arc<pirs_graph::LazyGraph>> = if cli.no_graph {
        None
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
        policy_hooks = if yolo {
            None
        } else {
            match (&ext_hooks.before_tool_call, &ext_hooks.after_tool_call) {
                (Some(b), Some(a)) => Some((b.clone(), a.clone())),
                _ => None,
            }
        };
        if let Some((b, a)) = &policy_hooks {
            let b_chained = pirs_agent::Hooks::chain_before(gate_hook.clone(), Some(b.clone()));
            if let Some(b_chained) = b_chained {
                *policy_slot.lock().unwrap() = Some((b_chained, a.clone()));
            }
        }
        if !yolo {
            hooks.before_tool_call =
                pirs_agent::Hooks::chain_before(gate_hook.clone(), ext_hooks.before_tool_call);
            let rhai_after = ext_hooks.after_tool_call;
            let graph_after = graph.clone().map(|g| {
                let g = std::sync::Arc::clone(&g);
                let g = std::sync::Arc::clone(&g);
                let cwd2 = cwd.clone();
                let f: pirs_agent::events::AfterToolCallHook =
                    std::sync::Arc::new(move |_id, name, result| {
                        if (name != "edit" && name != "write" && name != "ast_edit")
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

    if !cli.no_mcp {
        let mcp = pirs_mcp::load_servers(&cwd).await;
        for err in &mcp.errors {
            eprintln!("[mcp error] {err}");
        }
        if !mcp.handles.is_empty() {
            let names: Vec<String> = mcp.handles.iter().map(|h| h.name.clone()).collect();
            eprintln!("[mcp: {} ({} tools)]", names.join(", "), mcp.tools.len());
        }
        tools.extend(mcp.tools);
    }

    let skills = discovery::discover_skills(&cwd);
    let file_commands = discovery::discover_commands(&cwd);

    let mut system = system_prompt::build_system_prompt(&cwd, &tools);
    if let Some(block) = discovery::skills_prompt_block(&skills) {
        system.push_str(&block);
    }
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
    pirs_rhai::set_session_meta(
        &session_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        &cli.model,
    );
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
        let token = cli
            .serve_token
            .clone()
            .unwrap_or_else(|| format!("pirs-{}", pirs_ai::now_millis()));
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
        return tui::run(tui::TuiOptions {
            agent,
            host,
            session_path,
            approval_mode,
            approval_gate: Some(gate),
            cwd,
        })
        .await;
    }

    if let Some(prompt) = cli.prompt.first().cloned() {
        run_turn(
            &mut agent,
            &prompt,
            &printer,
            &session_path,
            approval_mode,
            host.as_ref(),
        )
        .await?;
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
                if let Err(e) = run_turn(agent, line, printer, &sp, mode, host).await {
                    eprintln!("[error: {e}]");
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => bail!(e),
        }
    }
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
                "/model [id]   show or set model\n\
                 /export <p>   export session to a JSONL file\n\
                 /compact      (not implemented) \n\
                 /quit         exit\n\
                 !<cmd>        run command locally, record output in context\n\
                 !!<cmd>       run command locally, do not record"
            );
        }
        "/model" => {
            if arg.is_empty() {
                println!("current model: {}", agent.model);
            } else {
                agent.model = arg.to_string();
                println!("model set to {arg}");
            }
        }
        "/usage" => {
            print_usage(&agent.usage_report());
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
