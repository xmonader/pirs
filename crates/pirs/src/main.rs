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
mod tui;
mod discovery;
mod session;
mod system_prompt;
mod rpc_mode;

#[derive(Parser)]
#[command(name = "pirs", about = "Rust port of the pi coding agent, extensible via rhai")]
struct Cli {
    /// One-shot prompt; if omitted, starts the interactive REPL
    prompt: Option<String>,

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

    /// API key (falls back to OPENAI_API_KEY)
    #[arg(long, env = "OPENAI_API_KEY")]
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

    /// Start with only core tools loaded; model loads more via use_tool
    #[arg(long)]
    tool_diet: bool,

    /// Execute tool calls one at a time (helps weaker models)
    #[arg(long)]
    sequential: bool,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let mut cli = Cli::parse();
    let cwd = std::env::current_dir()?;

    let api_key = cli
        .api_key
        .clone()
        .or_else(|| {
            if cli.provider == "anthropic" {
                std::env::var("ANTHROPIC_API_KEY").ok()
            } else {
                std::env::var("OPENAI_API_KEY").ok()
            }
        })
        .with_context(|| {
            if cli.provider == "anthropic" {
                "no API key: pass --api-key or set ANTHROPIC_API_KEY"
            } else {
                "no API key: pass --api-key or set OPENAI_API_KEY"
            }
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
            pirs_ai::AnthropicClient::new(cli.base_url.clone())
                .with_max_retries(cli.max_retries),
        )
    } else if cli.provider == "openai" {
        Arc::new(
            OpenAiCompat::new(cli.base_url.clone())
                .with_max_retries(cli.max_retries)
                .with_cache_key(format!("pirs-{}-{}", std::process::id(), cli.model)),
        )
    } else {
        anyhow::bail!("unknown provider '{}' (expected openai|anthropic)", cli.provider);
    };
    let usage_slot: std::sync::Arc<std::sync::Mutex<pirs_ai::Usage>> =
        std::sync::Arc::new(std::sync::Mutex::new(pirs_ai::Usage::default()));

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let mut hooks = Hooks::default();
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
        let runner_provider = std::sync::Arc::clone(&provider);
        let runner_completion = CompletionOptions {
            api_key: cli.api_key.clone().or_else(|| std::env::var("OPENAI_API_KEY").ok()),
            ..Default::default()
        };
        let runner_model = cli.model.clone();
        let runner_cwd = cwd.clone();
        let policy_slot_run = std::sync::Arc::clone(&policy_slot);
        let usage_slot_run = std::sync::Arc::clone(&usage_slot);
        h.set_subagent_runner(std::sync::Arc::new(
            move |task: String, model: Option<String>| {
                let provider = std::sync::Arc::clone(&runner_provider);
                let completion = runner_completion.clone();
                let cwd = runner_cwd.clone();
                let model = model.unwrap_or_else(|| runner_model.clone());
                let policy = policy_slot_run.lock().unwrap().clone();
                let usage_slot = usage_slot_run.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| e.to_string())?;
                    rt.block_on(async move {
                        let mut hooks = pirs_agent::Hooks::default();
                        if let Some((b, a)) = &policy {
                            hooks.before_tool_call = Some(b.clone());
                            hooks.after_tool_call = Some(a.clone());
                        }
                        let mut agent = pirs_agent::Agent::new(provider, &model)
                            .with_tools(pirs_tools::default_tools(cwd))
                            .with_completion(completion)
                            .with_hooks(hooks)
                            .with_compaction(None);
                        let new = agent.prompt(&task).await.map_err(|e| e.to_string())?;
                        *usage_slot.lock().unwrap() += agent.usage_report().grand_total();
                        new.iter()
                            .rev()
                            .find_map(|m| match m {
                                pirs_ai::Message::Assistant(a) if !a.text().trim().is_empty() => {
                                    Some(a.text())
                                }
                                _ => None,
                            })
                            .ok_or_else(|| "sub-agent produced no answer".to_string())
                    })
                })
                .join()
                .unwrap_or_else(|_| Err("sub-agent thread panicked".to_string()))
            },
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
        let yolo = approval::ApprovalMode::parse(&cli.approval) == Some(approval::ApprovalMode::Yolo);
        policy_hooks = if yolo {
            None
        } else {
            match (&ext_hooks.before_tool_call, &ext_hooks.after_tool_call) {
            (Some(b), Some(a)) => Some((b.clone(), a.clone())),
            _ => None,
            }
        };
        *policy_slot.lock().unwrap() = policy_hooks.clone();
        if !yolo {
            hooks.before_tool_call = ext_hooks.before_tool_call;
            hooks.after_tool_call = ext_hooks.after_tool_call;
        }
        hooks.transform_context = ext_hooks.transform_context;
        hooks.should_stop_after_turn = ext_hooks.should_stop_after_turn;
        hooks.get_steering_messages = ext_hooks.get_steering_messages;
        hooks.get_follow_up_messages = ext_hooks.get_follow_up_messages;
        Some(h)
    };

    if !cli.no_mcp {
        let mcp = pirs_mcp::load_servers(&cwd).await;
        for err in &mcp.errors {
            eprintln!("[mcp error] {err}");
        }
        if !mcp.handles.is_empty() {
            let names: Vec<String> = mcp
                .handles
                .iter()
                .map(|h| h.name.clone())
                .collect();
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
        api_key: Some(api_key),
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
        let delegate_provider: std::sync::Arc<dyn pirs_ai::LlmProvider> = if cli.provider == "anthropic" {
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
            api_key: cli.api_key.clone().or_else(|| std::env::var("OPENAI_API_KEY").ok()),
            ..Default::default()
        };
        let delegate_model = cli.model.clone();
        let delegate_cwd = cwd.clone();
        let delegate = pirs_agent::delegate::DelegateTool::new(
            delegate_provider,
            delegate_model,
            delegate_completion,
            move || pirs_tools::default_tools(delegate_cwd.clone()),
        );
        if let Some((b, a)) = &policy_hooks {
            delegate.with_policy_hooks(b.clone(), a.clone());
        }
        tools.push(delegate);
    }

    let (visible, mut tools) = if cli.tool_diet {
        let set: pirs_agent::agent_loop::VisibleTools = std::sync::Arc::new(
            std::sync::Mutex::new(pirs_agent::use_tool::UseTool::default_visible()),
        );
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

    let approval_mode = approval::ApprovalMode::parse(&cli.approval)
        .unwrap_or_else(|| {
            eprintln!("[unknown approval mode '{}', using auto]", cli.approval);
            approval::ApprovalMode::Auto
        });
    if approval_mode == approval::ApprovalMode::Yolo {
        eprintln!("[WARNING: yolo mode — no approvals, no policy hooks. All tool calls execute.]");
    }
    let gate = std::sync::Arc::new(approval::ApprovalGate::new(approval_mode, cwd.clone()));
    if approval_mode == approval::ApprovalMode::Ask {
        hooks.before_tool_call = Some(gate.hook());
    }

    let mut agent = Agent::new(provider, &cli.model)
        .with_system_prompt(system)
        .with_tools(tools)
        .with_completion(completion)
        .with_hooks(hooks)
        .with_compaction(compaction)
        .with_visible_tools(visible)
        .with_tool_execution(execution);
    agent.set_extra_usage_handle(usage_slot.clone());
    {
        let steer = agent.steer_sender();
        pirs_agent::jobs::registry().set_notifier(std::sync::Arc::new(move |msg| {
            steer(Message::user(msg));
        }));
    }
    let approval_shared = gate.shared_mode();

    let session_path = session::session_path(&cwd)?;
    if cli.resume {
        match session::load_latest(&cwd) {
            Ok((path, messages)) => {
                eprintln!("[resumed {} ({} messages)]", path.display(), messages.len());
                agent.messages = messages;
            }
            Err(e) => eprintln!("[no session to resume: {e}]"),
        }
    }

    let printer = Arc::new(Printer::new());
    if let Some(h) = &host {
        if let Some(l) = h.listener() {
            agent.subscribe(l);
        }
    }
    let printed = Arc::new(Mutex::new(0usize));
    {
        let p = Arc::clone(&printer);
        let printed = Arc::clone(&printed);
        agent.subscribe(Arc::new(move |event: AgentEvent| {
            match &event {
                AgentEvent::MessageStart { message } if message.is_assistant() => {
                    *printed.lock().unwrap() = 0;
                }
                AgentEvent::MessageUpdate { message } => {
                    let text = message.text();
                    let mut n = printed.lock().unwrap();
                    if text.len() > *n {
                        print!("{}", &text[*n..]);
                        let _ = std::io::stdout().flush();
                        *n = text.len();
                    }
                }
                _ => p.event(event),
            }
        }));
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

    if let Some(prompt) = cli.prompt.take() {
        run_turn(&mut agent, &prompt, &printer, &session_path, approval_mode, host.as_ref()).await?;
        eprintln!();
        print_usage(&agent.usage_report());
        return Ok(());
    }

    repl(&mut agent, &printer, &session_path, &cwd, host.as_ref(), &file_commands, approval_shared).await
}

async fn run_turn(
    agent: &mut Agent,
    input: &str,
    _printer: &Arc<Printer>,
    session_path: &Path,
    approval_mode: approval::ApprovalMode,
    host: Option<&std::sync::Arc<pirs_rhai::ExtensionHost>>,
) -> anyhow::Result<()> {
    let session_file = session_path.to_path_buf();
    agent.subscribe(Arc::new(move |event: pirs_agent::AgentEvent| {
        if let pirs_agent::AgentEvent::MessageEnd { message } = event {
            let _ = session::append(&session_file, &[*message]);
        }
    }));
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
                cancel.cancel();
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
    session_path: &Path,
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
                    match handle_command(line, agent, session_path, host, file_commands, printer, &approval_shared).await {
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
                if let Err(e) = run_turn(agent, line, printer, session_path, mode, host).await {
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
    session_path: &Path,
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
        "/export" => {
            if arg.is_empty() {
                bail!("usage: /export <path>");
            }
            let dest = PathBuf::from(arg);
            std::fs::copy(session_path, &dest)
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
                    if let Err(e) = run_turn(agent, &prompt, printer, session_path, mode, host).await {
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
