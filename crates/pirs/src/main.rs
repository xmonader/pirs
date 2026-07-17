use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use clap::Parser;
use pirs_agent::{Agent, AgentEvent, AgentTool, Hooks};
use pirs_ai::{CompletionOptions, Message, OpenAiCompat};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

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

    /// Retry failed/rate-limited requests up to N times
    #[arg(long, default_value = "0")]
    max_retries: u32,

    /// Disable automatic context compaction
    #[arg(long)]
    no_compaction: bool,

    /// Model context window in tokens (drives compaction threshold)
    #[arg(long, default_value = "128000")]
    context_window: u64,
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
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .context("no API key: pass --api-key or set OPENAI_API_KEY")?;

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
    if cli.mode != "repl" {
        bail!("unknown mode: {} (expected repl|rpc)", cli.mode);
    }

    let provider = Arc::new(
        OpenAiCompat::new(cli.base_url.clone()).with_max_retries(cli.max_retries),
    );

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let mut hooks = Hooks::default();

    let host = if cli.no_extensions {
        None
    } else {
        let mut h = pirs_rhai::ExtensionHost::new();
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
        hooks.before_tool_call = ext_hooks.before_tool_call;
        hooks.after_tool_call = ext_hooks.after_tool_call;
        hooks.transform_context = ext_hooks.transform_context;
        hooks.should_stop_after_turn = ext_hooks.should_stop_after_turn;
        hooks.get_steering_messages = ext_hooks.get_steering_messages;
        hooks.get_follow_up_messages = ext_hooks.get_follow_up_messages;
        Some(h)
    };

    let mut system = system_prompt::build_system_prompt(&cwd, &tools);
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

    let mut agent = Agent::new(provider, &cli.model)
        .with_system_prompt(system)
        .with_tools(tools)
        .with_completion(completion)
        .with_hooks(hooks)
        .with_compaction(compaction);

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

    if let Some(prompt) = cli.prompt.take() {
        run_turn(&mut agent, &prompt, &printer, &session_path).await?;
        return Ok(());
    }

    repl(&mut agent, &printer, &session_path, &cwd).await
}

async fn run_turn(
    agent: &mut Agent,
    input: &str,
    _printer: &Arc<Printer>,
    session_path: &Path,
) -> anyhow::Result<()> {
    let cancel = agent.cancel_handle();
    let steer_handle = SteerHandle::start(agent);

    let mut run = std::pin::pin!(agent.prompt(input));
    let result = loop {
        tokio::select! {
            r = &mut run => break r,
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
            }
        }
    };
    steer_handle.stop();

    let new_messages = result?;
    session::append(session_path, &new_messages)?;
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
                    match handle_command(line, agent, session_path).await {
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
                if let Err(e) = run_turn(agent, line, printer, session_path).await {
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

async fn handle_command(line: &str, agent: &mut Agent, session_path: &Path) -> anyhow::Result<bool> {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "/quit" | "/exit" => return Ok(true),
        "/help" => {
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
        other => println!("unknown command: {other}"),
    }
    Ok(false)
}
