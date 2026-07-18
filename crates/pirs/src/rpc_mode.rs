use std::path::PathBuf;
use std::sync::Arc;

use pirs_agent::{Agent, AgentEvent, AgentTool, Hooks, QueueMode};
use pirs_ai::{CompletionOptions, Message};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::session;
use crate::system_prompt;

pub struct RpcOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: String,
    pub max_retries: u32,
}

type RunOutput = (
    Vec<Message>,
    Vec<Message>,
    Option<pirs_agent::agent_loop::BudgetHit>,
);
type RunFuture = std::pin::Pin<Box<dyn std::future::Future<Output = RunOutput> + Send>>;

struct Actor {
    agent: Agent,
    session_path: PathBuf,
    session_slot: Arc<std::sync::Mutex<PathBuf>>,
    session_id: String,
    cwd: PathBuf,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    run: Option<RunFuture>,
}

enum Cmd {
    Line(Value),
    RunFinished(
        Vec<Message>,
        Vec<Message>,
        Option<pirs_agent::agent_loop::BudgetHit>,
    ),
}

pub async fn run(opts: RpcOptions) -> anyhow::Result<()> {
    let cwd = &opts.cwd;
    let provider: std::sync::Arc<dyn pirs_ai::LlmProvider> = if std::env::var("PIRS_PROVIDER")
        .as_deref()
        == Ok("anthropic")
    {
        std::sync::Arc::new(
            pirs_ai::AnthropicClient::new(opts.base_url.clone()).with_max_retries(opts.max_retries),
        )
    } else {
        std::sync::Arc::new(
            pirs_ai::OpenAiCompat::new(opts.base_url.clone()).with_max_retries(opts.max_retries),
        )
    };

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let mut hooks = Hooks::default();
    let approval_mode = std::env::var("PIRS_APPROVAL")
        .ok()
        .and_then(|m| crate::approval::ApprovalMode::parse(&m))
        .unwrap_or(crate::approval::ApprovalMode::Auto);
    let gate = crate::approval::ApprovalGate::new(approval_mode, cwd.clone());
    let gate_hook = if approval_mode == crate::approval::ApprovalMode::Ask {
        Some(gate.hook())
    } else {
        None
    };
    let mut host = pirs_rhai::ExtensionHost::new();
    let policy_slot: std::sync::Arc<
        std::sync::Mutex<
            Option<(
                pirs_agent::events::BeforeToolCallHook,
                pirs_agent::events::AfterToolCallHook,
            )>,
        >,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let runner_cwd = cwd.clone();
    host.set_subagent_runner(crate::subagent::build_subagent_runner(
        std::sync::Arc::clone(&provider),
        CompletionOptions {
            api_key: Some(opts.api_key.clone()),
            ..Default::default()
        },
        opts.model.clone(),
        pirs_tools::default_tools(runner_cwd),
        std::sync::Arc::clone(&policy_slot),
        std::sync::Arc::new(std::sync::Mutex::new(pirs_ai::Usage::default())),
    ));
    host.load_default_dirs(cwd);
    for err in &host.load_errors {
        eprintln!("[extension error] {err}");
    }
    let host = Arc::new(host);
    tools.extend(host.tools());
    let ext_hooks = host.hooks();

    // Give sub-agents the same policy chain as the main agent (gate + before,
    // after). Previously this slot was declared but never populated, so RPC
    // sub-agents ran with no policy hooks at all.
    if let (Some(b), Some(a)) = (&ext_hooks.before_tool_call, &ext_hooks.after_tool_call) {
        if let Some(chained) = pirs_agent::Hooks::chain_before(gate_hook.clone(), Some(b.clone())) {
            *policy_slot.lock().unwrap() = Some((chained, a.clone()));
        }
    }

    hooks.before_tool_call = pirs_agent::Hooks::chain_before(gate_hook, ext_hooks.before_tool_call);
    hooks.after_tool_call = ext_hooks.after_tool_call;
    hooks.transform_context = ext_hooks.transform_context;
    hooks.should_stop_after_turn = ext_hooks.should_stop_after_turn;
    hooks.get_steering_messages = ext_hooks.get_steering_messages;
    hooks.get_follow_up_messages = ext_hooks.get_follow_up_messages;

    let mut system = system_prompt::build_system_prompt(cwd, &tools);
    if let Some(ctx) = system_prompt::read_project_context(cwd) {
        system.push_str(&ctx);
    }

    let completion = CompletionOptions {
        api_key: Some(opts.api_key.clone()),
        ..Default::default()
    };
    let _ = &opts;

    let mut agent = Agent::new(provider, &opts.model)
        .with_system_prompt(system)
        .with_tools(tools)
        .with_completion(completion)
        .with_hooks(hooks)
        .with_compaction(Some(pirs_agent::compaction::CompactionConfig::default()));

    let session_path = session::session_path(cwd)?;
    let session_id = session_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    pirs_rhai::set_session_meta(&session_id, &opts.model);

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    {
        let out = out_tx.clone();
        agent.subscribe(Arc::new(move |event: AgentEvent| {
            let line = serde_json::to_string(&event).unwrap_or_default();
            let _ = out.send(format!("{line}\n"));
        }));
    }
    // Single subscriber, gated on the CURRENT session path via a slot:
    // new_session updates the slot instead of stacking a second
    // subscriber (which double-writes every message to both files).
    let session_slot = Arc::new(std::sync::Mutex::new(session_path.clone()));
    {
        let slot = Arc::clone(&session_slot);
        agent.subscribe(Arc::new(move |event: AgentEvent| {
            if let AgentEvent::MessageEnd { message } = event {
                let path = slot.lock().unwrap().clone();
                let _ = session::append(&path, &[*message]);
            }
        }));
    }
    if let Some(l) = host.listener() {
        agent.subscribe(l);
    }

    let mut actor = Actor {
        agent,
        session_path,
        session_slot,
        session_id,
        cwd: cwd.clone(),
        steering_mode: QueueMode::default(),
        follow_up_mode: QueueMode::default(),
        run: None,
    };

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let cmd = tokio::select! {
            line = stdin.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        let l = l.trim().to_string();
                        if l.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(&l) {
                            Ok(v) => Cmd::Line(v),
                            Err(e) => {
                                send(&out_tx, json!({
                                    "type": "response",
                                    "success": false,
                                    "error": format!("invalid json: {e}"),
                                }));
                                continue;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            result = async {
                match actor.run.as_mut() {
                    Some(f) => f.await,
                    None => std::future::pending().await,
                }
            } => {
                Cmd::RunFinished(result.0, result.1, result.2)
            }
        };

        match cmd {
            Cmd::RunFinished(full, new, hit) => {
                actor.run = None;
                actor.agent.budget_hit = hit;
                actor.agent.complete_run(full);
                let _ = &new;
            }
            Cmd::Line(v) => {
                handle_command(v, &mut actor, &out_tx).await;
            }
        }
    }
    Ok(())
}

fn send(out: &tokio::sync::mpsc::UnboundedSender<String>, value: Value) {
    let _ = out.send(format!("{value}\n"));
}

async fn handle_command(
    cmd: Value,
    actor: &mut Actor,
    out: &tokio::sync::mpsc::UnboundedSender<String>,
) {
    let id = cmd
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let ty = cmd
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let respond = |success: bool, data: Option<Value>, error: Option<String>| {
        let mut r = json!({
            "type": "response",
            "command": ty,
            "success": success,
        });
        if let Some(id) = &id {
            r["id"] = json!(id);
        }
        if let Some(d) = data {
            r["data"] = d;
        }
        if let Some(e) = error {
            r["error"] = json!(e);
        }
        send(out, r);
    };

    let msg_arg = || {
        cmd.get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    match ty.as_str() {
        "prompt" => {
            let message = msg_arg();
            let follow_up = matches!(
                cmd.get("streamingBehavior").and_then(|v| v.as_str()),
                Some("followUp")
            );
            if actor.run.is_some() {
                if follow_up {
                    actor.agent.follow_up(Message::user(message));
                } else {
                    actor.agent.steer(Message::user(message));
                }
                respond(true, None, None);
            } else {
                respond(true, None, None);
                match actor.agent.begin_prompt(vec![Message::user(message)]) {
                    Ok(fut) => actor.run = Some(Box::pin(fut)),
                    Err(e) => {
                        let msg = e.to_string();
                        respond(false, None, Some(msg));
                    }
                }
            }
        }
        "steer" => {
            actor.agent.steer(Message::user(msg_arg()));
            respond(true, None, None);
        }
        "follow_up" => {
            actor.agent.follow_up(Message::user(msg_arg()));
            respond(true, None, None);
        }
        "abort" => {
            actor.agent.cancel();
            respond(true, None, None);
        }
        "new_session" => {
            let cancelled = actor.agent.is_running();
            actor.agent.cancel();
            actor.agent.messages.clear();
            match session::session_path(&actor.cwd) {
                Ok(new_path) => {
                    actor.session_path = new_path.clone();
                    // Redirect the single session subscriber to the new file.
                    *actor.session_slot.lock().unwrap() = new_path;
                }
                Err(e) => tracing::warn!("failed to rotate session: {e}"),
            }
            respond(true, Some(json!({"cancelled": cancelled})), None);
        }
        "get_state" => {
            respond(
                true,
                Some(json!({
                    "model": { "provider": "openai", "id": actor.agent.model },
                    "thinkingLevel": "off",
                    "isStreaming": actor.agent.is_running(),
                    "isCompacting": false,
                    "steeringMode": queue_mode_name(actor.steering_mode),
                    "followUpMode": queue_mode_name(actor.follow_up_mode),
                    "sessionFile": actor.session_path.to_string_lossy(),
                    "sessionId": actor.session_id,
                    "autoCompactionEnabled": actor.agent.compaction_enabled(),
                    "messageCount": actor.agent.messages.len(),
                    "pendingMessageCount": 0,
                })),
                None,
            );
        }
        "set_model" => {
            let model = cmd
                .get("modelId")
                .or_else(|| cmd.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if model.is_empty() {
                respond(false, None, Some("set_model requires modelId".into()));
            } else {
                actor.agent.model = model.to_string();
                respond(true, None, None);
            }
        }
        "set_steering_mode" | "set_follow_up_mode" => {
            let mode = cmd.get("mode").and_then(|v| v.as_str()).unwrap_or("");
            let parsed = match mode {
                "all" => Some(QueueMode::All),
                "one-at-a-time" => Some(QueueMode::OneAtATime),
                _ => None,
            };
            match parsed {
                Some(m) => {
                    if ty == "set_steering_mode" {
                        actor.steering_mode = m;
                    } else {
                        actor.follow_up_mode = m;
                    }
                    respond(true, None, None);
                }
                None => respond(false, None, Some(format!("invalid mode: {mode}"))),
            }
        }
        "get_messages" => {
            let msgs = serde_json::to_value(&actor.agent.messages).unwrap_or(Value::Null);
            respond(true, Some(json!({ "messages": msgs })), None);
        }
        "get_last_assistant_text" => {
            let text = actor
                .agent
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    Message::Assistant(a) => Some(a.text()),
                    _ => None,
                })
                .unwrap_or_default();
            respond(true, Some(json!({ "text": text })), None);
        }
        "get_session_stats" => {
            let report = actor.agent.usage_report();
            let total = report.grand_total();
            let by_model: serde_json::Map<String, Value> = report
                .by_model
                .iter()
                .map(|(m, u)| (m.clone(), serde_json::to_value(u).unwrap_or(Value::Null)))
                .collect();
            respond(
                true,
                Some(json!({
                    "sessionFile": actor.session_path.to_string_lossy(),
                    "sessionId": actor.session_id,
                    "messageCount": actor.agent.messages.len(),
                    "apiCalls": report.calls.len() - report.delegate_calls(),
                    "delegateCalls": report.delegate_calls(),
                    "usage": serde_json::to_value(&total).unwrap_or(Value::Null),
                    "usageByModel": Value::Object(by_model),
                })),
                None,
            );
        }
        "bash" => {
            let command = cmd
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let exclude = cmd
                .get("excludeFromContext")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tool = pirs_tools::BashTool::new(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            );
            let result = tool
                .execute(pirs_agent::ToolExecContext {
                    tool_call_id: format!("rpc-bash-{}", pirs_ai::now_millis()),
                    args: json!({"command": command}),
                    cancel: tokio_util::sync::CancellationToken::new(),
                    on_update: None,
                })
                .await;
            match result {
                Ok(o) => {
                    let text = o
                        .content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !exclude {
                        actor.agent.messages.push(Message::user(format!(
                            "User ran a local command: `{command}`\nOutput:\n{text}"
                        )));
                    }
                    respond(
                        true,
                        Some(json!({
                            "output": text,
                            "exitCode": 0,
                            "cancelled": false,
                            "truncated": false,
                        })),
                        None,
                    );
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !exclude {
                        actor.agent.messages.push(Message::user(format!(
                            "User ran a local command: `{command}`\nOutput:\n{msg}"
                        )));
                    }
                    respond(
                        true,
                        Some(json!({
                            "output": msg,
                            "exitCode": 1,
                            "cancelled": false,
                            "truncated": false,
                        })),
                        None,
                    );
                }
            }
        }
        other => respond(false, None, Some(format!("unsupported command: {other}"))),
    }
}

fn queue_mode_name(m: QueueMode) -> &'static str {
    match m {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}
