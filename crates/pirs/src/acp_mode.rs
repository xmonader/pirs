//! ACP (Agent Client Protocol — <https://agentclientprotocol.com>) support,
//! `--mode acp`: lets editors that embed ACP agents (Zed, and others as the
//! ecosystem grows) drive pirs directly instead of going through a terminal.
//!
//! Wire format, confirmed against the real `acp` Python SDK vendored
//! locally (`agent-client-protocol`, schema ref `v0.11.2`) rather than
//! guessed: JSON-RPC 2.0 over newline-delimited JSON on stdio, structurally
//! the same shape as pirs's own `--mode rpc`, but with a real JSON-RPC
//! envelope (`jsonrpc`/`id`/`method`/`params`/`result`/`error`) instead of
//! that mode's flat ad-hoc `{"type": ...}` protocol.
//!
//! Working subset (expanded):
//!   - Agent-side methods: `initialize`, `session/new`, `session/prompt`,
//!     `session/cancel`, plus client-capability **fs** helpers we *call*
//!     when the client advertises them: `fs/read_text_file` /
//!     `fs/write_text_file` (optional bridge so unsaved buffers can be
//!     preferred over disk). We still read/write the real filesystem from
//!     tools when the client has no fs capability.
//!   - Client-side methods called: `session/update` (streamed notifications:
//!     `agent_message_chunk` for assistant text, `tool_call`/
//!     `tool_call_update` for tool execution) and `session/request_permission`.
//!   - Prompt capabilities: **image** blocks in `session/prompt` are accepted
//!     (base64 → multimodal user message).
//!   - **Not implemented**: `terminal/*`, `session/load`, `authenticate`,
//!     multiple concurrent sessions (a second `session/new` replaces the
//!     current session).
//!
//! `PermissionOption`s offered are just `allow`/`deny` (`allow_once`/
//! `reject_once`) — no persistent "always allow this bucket" memory across
//! calls, unlike the REPL/TUI's `ApprovalGate`. This mode intentionally
//! does not reuse `ApprovalGate`: its `before_tool_call` hook is
//! synchronous/blocking (`Fn(&str, &str, &Value) -> Option<String>`, same
//! as `ApprovalGate::hook()`'s), but needs the tool name/args to build a
//! structured ACP `ToolCallUpdate`, not a pre-flattened question string —
//! so it blocks on a `std::sync::mpsc::Receiver` the same way the TUI's
//! `approval_bridge` does, just answered by a `session/request_permission`
//! round trip instead of a keypress.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pirs_agent::{Agent, AgentEvent, AgentTool, Hooks};
use pirs_ai::{CompletionOptions, Message};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::session;
use crate::system_prompt;

pub struct AcpOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: String,
    pub max_retries: u32,
    /// Resolved CLI provider (`openai` | `anthropic`) — not re-read from env alone.
    pub provider: String,
    pub approval: String,
    pub agent_profile: String,
    pub permission_mode: Option<String>,
}

const PROTOCOL_VERSION: u64 = 1;

type RunOutput = (
    Vec<Message>,
    Vec<Message>,
    Option<pirs_agent::agent_loop::BudgetHit>,
);
/// Completion channel for a turn running on a **separate** tokio task.
/// Permission hooks block that task on std::sync::mpsc; the stdin select loop
/// must stay free to deliver client permission replies into the pending map.
type RunWait = tokio::sync::oneshot::Receiver<RunOutput>;

/// Senders waiting on a response to a request *we* sent to the client
/// (currently only `session/request_permission`), keyed by the outbound
/// request id we generated. The stdin reader loop resolves these when a
/// matching `{"id": ..., "result"/"error": ...}` line arrives with no
/// `method` field (which is how a response is told apart from an incoming
/// request/notification in JSON-RPC).
type PendingMap = Arc<Mutex<HashMap<u64, std::sync::mpsc::Sender<Value>>>>;

struct SessionState {
    agent: Agent,
    session_id: String,
    run: Option<RunWait>,
    /// The `session/prompt` request id to answer once `run` resolves —
    /// `session/prompt` doesn't respond until the whole turn (including any
    /// steering/tool calls) finishes.
    pending_prompt_id: Option<Value>,
    /// Multi-session: message history keyed by sessionId (in-memory).
    session_histories: HashMap<String, Vec<Message>>,
}

pub async fn run(opts: AcpOptions) -> anyhow::Result<()> {
    let cwd = opts.cwd.clone();
    let provider: Arc<dyn pirs_ai::LlmProvider> =
        if crate::runtime_safety::provider_is_anthropic(&opts.provider) {
            Arc::new(
                pirs_ai::AnthropicClient::new(opts.base_url.clone())
                    .with_max_retries(opts.max_retries),
            )
        } else {
            Arc::new(
                pirs_ai::OpenAiCompat::new(opts.base_url.clone())
                    .with_max_retries(opts.max_retries),
            )
        };

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = out_rx.recv().await {
            let line = format!("{value}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let next_out_id = Arc::new(AtomicU64::new(1));
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let session_id_slot: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let safety_cfg = crate::runtime_safety::SafetyConfig::from_resolved(
        cwd.clone(),
        &opts.approval,
        &opts.agent_profile,
        opts.permission_mode.as_deref(),
        &opts.provider,
    );

    let acp_perm = acp_permission_hook(
        Arc::clone(&pending),
        out_tx.clone(),
        Arc::clone(&next_out_id),
        Arc::clone(&session_id_slot),
    );

    let mut host = pirs_rhai::ExtensionHost::new();
    let policy_slot: std::sync::Arc<
        std::sync::Mutex<
            Option<(
                pirs_agent::events::BeforeToolCallHook,
                pirs_agent::events::AfterToolCallHook,
            )>,
        >,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    host.set_subagent_runner(crate::subagent::build_subagent_runner(
        Arc::clone(&provider),
        CompletionOptions {
            api_key: Some(opts.api_key.clone()),
            ..Default::default()
        },
        opts.model.clone(),
        pirs_tools::default_tools(cwd.clone()),
        Arc::clone(&policy_slot),
        Arc::new(Mutex::new(pirs_ai::Usage::default())),
    ));
    // Pack set from built-in `default` profile (`packs: "*"`).
    if let Ok(p) = pirs_rhai::discover::resolve_pack_profile(None, &cwd) {
        pirs_rhai::weak_packs::load_profile_packs(&mut host, &p.packs);
    } else {
        pirs_rhai::weak_packs::load_into(&mut host);
    }
    host.load_default_dirs(&cwd);
    for err in &host.load_errors {
        eprintln!("[extension error] {err}");
    }
    let host = Arc::new(host);
    tools.extend(host.tools());
    let ext_hooks = host.hooks();

    let mut system = system_prompt::build_system_prompt(&cwd, &tools);
    if let Some(ctx) = system_prompt::read_project_context(&cwd) {
        system.push_str(&ctx);
    }

    let completion = CompletionOptions {
        api_key: Some(opts.api_key.clone()),
        ..Default::default()
    };

    let mut agent = Agent::new(provider, &opts.model)
        .with_system_prompt(system)
        .with_tools(tools)
        .with_completion(completion)
        .with_compaction(Some(pirs_agent::compaction::CompactionConfig::default()));

    // Shared safety floor + ACP client permission + extension hooks.
    let mut ext_for_install = Hooks::default();
    ext_for_install.after_tool_call = ext_hooks.after_tool_call.clone();
    ext_for_install.transform_context = ext_hooks.transform_context.clone();
    ext_for_install.should_stop_after_turn = ext_hooks.should_stop_after_turn.clone();
    ext_for_install.get_steering_messages = ext_hooks.get_steering_messages.clone();
    ext_for_install.get_follow_up_messages = ext_hooks.get_follow_up_messages.clone();
    // Chain: safety gate → ACP permission → extension before hooks
    let before_ext = pirs_agent::Hooks::chain_before(
        Some(acp_perm),
        ext_hooks.before_tool_call.clone(),
    );
    agent = crate::runtime_safety::install_safety_floor(
        agent,
        &safety_cfg,
        before_ext,
        ext_for_install,
    );

    // Sub-agents always get profile/permission gate (even with no pack hooks).
    crate::runtime_safety::fill_subagent_policy_slot(
        &policy_slot,
        &safety_cfg,
        ext_hooks.before_tool_call.clone(),
        ext_hooks.after_tool_call.clone(),
    );

    let session_path = session::session_path(&cwd)?;
    let session_id = session_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    if let Err(e) = pirs_agent::memory::init_global(&cwd.join(".pirs").join("memory.db")) {
        eprintln!("[memory disabled: {e}]");
    } else {
        pirs_agent::memory::set_session(&session_id);
    }
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

    let last_text_len: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    {
        let out = out_tx.clone();
        let session_id_slot = Arc::clone(&session_id_slot);
        let last_text_len = Arc::clone(&last_text_len);
        agent.subscribe(Arc::new(move |event: AgentEvent| {
            let session_id = session_id_slot.lock().unwrap().clone();
            if session_id.is_empty() {
                return;
            }
            emit_session_update(&out, &session_id, &last_text_len, event);
        }));
    }

    let mut state = SessionState {
        agent,
        session_id: String::new(),
        run: None,
        pending_prompt_id: None,
        session_histories: HashMap::new(),
    };

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    loop {
        enum Cmd {
            Incoming(Value),
            RunFinished(RunOutput),
        }
        let cmd = tokio::select! {
            line = stdin.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        let l = l.trim();
                        if l.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(l) {
                            Ok(v) => Cmd::Incoming(v),
                            Err(_) => continue,
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            result = async {
                match state.run.as_mut() {
                    Some(rx) => match rx.await {
                        Ok(out) => out,
                        Err(_) => {
                            // Task dropped — synthesize empty finish.
                            (vec![], vec![], None)
                        }
                    },
                    None => std::future::pending().await,
                }
            } => Cmd::RunFinished(result),
        };

        match cmd {
            Cmd::RunFinished((full, _new, hit)) => {
                state.run = None;
                state.agent.budget_hit = hit;
                state.agent.complete_run(full);
                // Keep multi-session map in sync after each turn.
                if !state.session_id.is_empty() {
                    state
                        .session_histories
                        .insert(state.session_id.clone(), state.agent.messages.clone());
                }
                if let Some(id) = state.pending_prompt_id.take() {
                    respond(&out_tx, Some(id), json!({"stopReason": "end_turn"}));
                }
            }
            Cmd::Incoming(v) => {
                if let Some(method) = v.get("method").and_then(|m| m.as_str()).map(str::to_string) {
                    let id = v.get("id").cloned();
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    handle_method(&method, id, params, &mut state, &out_tx, &session_id_slot).await;
                } else if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                    // A response to one of OUR outbound requests
                    // (session/request_permission).
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let result = v.get("result").cloned().unwrap_or(Value::Null);
                        let _ = tx.send(result);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_method(
    method: &str,
    id: Option<Value>,
    params: Value,
    state: &mut SessionState,
    out: &tokio::sync::mpsc::UnboundedSender<Value>,
    session_id_slot: &Arc<Mutex<String>>,
) {
    match method {
        "initialize" => {
            // Remember client capabilities (fs bridge) if present.
            if let Some(caps) = params.get("clientCapabilities") {
                if let Ok(mut slot) = client_caps().lock() {
                    *slot = caps.clone();
                }
            }
            respond(
                out,
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": {
                            "image": true,
                            "audio": false,
                            "embeddedContext": true,
                        },
                    },
                    "agentInfo": {
                        "name": "pirs",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "authMethods": [],
                }),
            );
        }
        "session/new" => {
            // Persist current session history before switching.
            if !state.session_id.is_empty() {
                state
                    .session_histories
                    .insert(state.session_id.clone(), state.agent.messages.clone());
            }
            let session_id = format!("acp-{}-{}", std::process::id(), pirs_ai::now_millis());
            *session_id_slot.lock().unwrap() = session_id.clone();
            state.session_id = session_id.clone();
            state.agent.messages.clear();
            respond(out, id, json!({"sessionId": session_id}));
        }
        "session/load" => {
            if state.run.is_some() {
                respond_error(out, id, -32000, "cannot load while a turn is in progress");
                return;
            }
            let sid = params
                .get("sessionId")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if sid.is_empty() {
                respond_error(out, id, -32602, "sessionId required");
                return;
            }
            // Save current.
            if !state.session_id.is_empty() {
                state
                    .session_histories
                    .insert(state.session_id.clone(), state.agent.messages.clone());
            }
            let msgs = state
                .session_histories
                .get(&sid)
                .cloned()
                .unwrap_or_default();
            state.agent.messages = msgs;
            state.session_id = sid.clone();
            *session_id_slot.lock().unwrap() = sid.clone();
            respond(
                out,
                id,
                json!({
                    "sessionId": sid,
                    "messageCount": state.agent.messages.len(),
                }),
            );
        }
        "session/prompt" => {
            if state.run.is_some() {
                respond_error(
                    out,
                    id,
                    -32000,
                    "a turn is already in progress for this session",
                );
                return;
            }
            // Optional sessionId routes to the right history.
            if let Some(sid) = params.get("sessionId").and_then(|s| s.as_str()) {
                if sid != state.session_id {
                    if !state.session_id.is_empty() {
                        state
                            .session_histories
                            .insert(state.session_id.clone(), state.agent.messages.clone());
                    }
                    if let Some(msgs) = state.session_histories.get(sid) {
                        state.agent.messages = msgs.clone();
                    } else {
                        state.agent.messages.clear();
                    }
                    state.session_id = sid.to_string();
                    *session_id_slot.lock().unwrap() = sid.to_string();
                }
            }
            let user_msg = extract_prompt_message(&params);
            state.pending_prompt_id = id;
            match state.agent.begin_prompt(vec![user_msg]) {
                Ok(fut) => {
                    // Run the agent on a separate task so blocking permission
                    // hooks cannot starve the stdin reader (which delivers the
                    // permission response into `pending`).
                    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                    tokio::spawn(async move {
                        let out = fut.await;
                        let _ = done_tx.send(out);
                    });
                    state.run = Some(done_rx);
                }
                Err(e) => {
                    let rid = state.pending_prompt_id.take();
                    respond_error(out, rid, -32000, &e.to_string());
                }
            }
        }
        "session/cancel" => {
            // A notification (no id, no response) per the ACP schema.
            state.agent.cancel();
        }
        // Client may call these on us if it treats the agent as an fs provider;
        // we also implement them so ACP tools/tests can round-trip.
        "fs/read_text_file" => {
            let path = params
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("");
            match std::fs::read_to_string(path) {
                Ok(content) => respond(out, id, json!({"content": content})),
                Err(e) => respond_error(out, id, -32000, &format!("read {path}: {e}")),
            }
        }
        "fs/write_text_file" => {
            let path = params
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("");
            let content = params
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if path.is_empty() {
                respond_error(out, id, -32602, "path required");
                return;
            }
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(path, content) {
                Ok(()) => respond(out, id, json!({})),
                Err(e) => respond_error(out, id, -32000, &format!("write {path}: {e}")),
            }
        }
        other => {
            if id.is_some() {
                respond_error(out, id, -32601, &format!("method not found: {other}"));
            }
        }
    }
}

/// Client capabilities from `initialize` (optional fs bridge).
static CLIENT_CAPS: std::sync::OnceLock<std::sync::Mutex<Value>> = std::sync::OnceLock::new();

fn client_caps() -> &'static std::sync::Mutex<Value> {
    CLIENT_CAPS.get_or_init(|| std::sync::Mutex::new(Value::Null))
}

/// Build a multimodal user message from ACP prompt blocks (text + image).
fn extract_prompt_message(params: &Value) -> Message {
    let blocks = params
        .get("prompt")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut content_blocks: Vec<pirs_ai::ContentBlock> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    for b in &blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t.to_string());
                    content_blocks.push(pirs_ai::ContentBlock::text(t));
                }
            }
            Some("image") => {
                // ACP image: { type, data (base64), mimeType } or data URL.
                let mime = b
                    .get("mimeType")
                    .or_else(|| b.get("mime_type"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png")
                    .to_string();
                if let Some(data) = b.get("data").and_then(|d| d.as_str()) {
                    content_blocks.push(pirs_ai::ContentBlock::Image {
                        data: data.to_string(),
                        mime_type: mime,
                    });
                }
            }
            Some("resource") | Some("resource_link") => {
                // Embedded context: surface path/uri as text for the model.
                if let Some(uri) = b
                    .get("uri")
                    .or_else(|| b.get("path"))
                    .and_then(|u| u.as_str())
                {
                    let note = format!("[context: {uri}]");
                    text_parts.push(note.clone());
                    content_blocks.push(pirs_ai::ContentBlock::text(note));
                }
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t.to_string());
                    content_blocks.push(pirs_ai::ContentBlock::text(t));
                }
            }
            _ => {}
        }
    }
    if content_blocks.is_empty() {
        return Message::user(text_parts.join("\n\n"));
    }
    // Prefer blocks form when any image present so providers get multimodal.
    let has_image = content_blocks
        .iter()
        .any(|c| matches!(c, pirs_ai::ContentBlock::Image { .. }));
    if has_image {
        Message::User(pirs_ai::UserMessage {
            content: pirs_ai::UserContent::Blocks(content_blocks),
            timestamp: pirs_ai::now_millis(),
        })
    } else {
        Message::user(text_parts.join("\n\n"))
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn extract_prompt_text(params: &Value) -> String {
    match extract_prompt_message(params) {
        Message::User(u) => match u.content {
            pirs_ai::UserContent::Text(t) => t,
            pirs_ai::UserContent::Blocks(bs) => bs
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n\n"),
        },
        _ => String::new(),
    }
}

fn respond(out: &tokio::sync::mpsc::UnboundedSender<Value>, id: Option<Value>, result: Value) {
    if let Some(id) = id {
        let _ = out.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

fn respond_error(
    out: &tokio::sync::mpsc::UnboundedSender<Value>,
    id: Option<Value>,
    code: i64,
    message: &str,
) {
    if let Some(id) = id {
        let _ = out.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }));
    }
}

/// Translates pirs's internal `AgentEvent` stream into ACP `session/update`
/// notifications. `last_text_len` tracks how much of the current message's
/// accumulated text has already been sent, since `MessageUpdate` carries the
/// full text-so-far rather than just the newest delta — `agent_message_chunk`
/// wants incremental pieces, not the whole thing repeated every time.
fn emit_session_update(
    out: &tokio::sync::mpsc::UnboundedSender<Value>,
    session_id: &str,
    last_text_len: &Mutex<usize>,
    event: AgentEvent,
) {
    match event {
        AgentEvent::MessageStart { .. } => {
            *last_text_len.lock().unwrap() = 0;
        }
        AgentEvent::MessageUpdate { message } => {
            let text = message.text();
            let mut last = last_text_len.lock().unwrap();
            if text.len() > *last {
                let delta = text[*last..].to_string();
                *last = text.len();
                drop(last);
                let _ = out.send(json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": delta},
                        },
                    },
                }));
            }
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            let _ = out.send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": tool_call_id,
                        "title": tool_name,
                        "status": "in_progress",
                        "rawInput": args,
                    },
                },
            }));
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            ..
        } => {
            let status = if result.is_error {
                "failed"
            } else {
                "completed"
            };
            let text = result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            let _ = out.send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": tool_call_id,
                        "status": status,
                        "rawOutput": text,
                    },
                },
            }));
        }
        _ => {}
    }
}

/// Every tool call is gated through the client via `session/request_permission`.
///
/// This is a synchronous `BeforeToolCallHook` that blocks **the agent task** on
/// a `std::sync::mpsc::Receiver` until the stdin reader resolves the matching
/// pending outbound request. The agent turn is spawned on a separate tokio task
/// so this block never freezes the stdin select loop.
fn acp_permission_hook(
    pending: PendingMap,
    out: tokio::sync::mpsc::UnboundedSender<Value>,
    next_id: Arc<AtomicU64>,
    session_id_slot: Arc<Mutex<String>>,
) -> pirs_agent::events::BeforeToolCallHook {
    Arc::new(move |tool_call_id, tool_name, args| {
        let session_id = session_id_slot.lock().unwrap().clone();
        if session_id.is_empty() {
            // No session yet (shouldn't happen once session/new has run) —
            // fail open would be wrong for an approval gate, so fail closed.
            return Some("acp: no active session to request permission on".to_string());
        }
        let req_id = next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        pending.lock().unwrap().insert(req_id, tx);

        let sent = out.send(json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": tool_call_id,
                    "title": tool_name,
                    "rawInput": args,
                },
                "options": [
                    {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
                ],
            },
        }));
        if sent.is_err() {
            pending.lock().unwrap().remove(&req_id);
            return Some("acp: could not reach client for permission".to_string());
        }

        match rx.recv() {
            Ok(response) => {
                let outcome = response.get("outcome").cloned().unwrap_or(Value::Null);
                match outcome.get("outcome").and_then(|v| v.as_str()) {
                    Some("selected") => match outcome.get("optionId").and_then(|v| v.as_str()) {
                        Some("allow") => None,
                        other => Some(format!(
                            "denied by client (optionId={})",
                            other.unwrap_or("<none>")
                        )),
                    },
                    _ => Some("cancelled by client".to_string()),
                }
            }
            Err(_) => Some("acp: permission channel closed before a response arrived".to_string()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv_all(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    #[test]
    fn extract_prompt_text_joins_text_blocks_and_ignores_others() {
        let params = json!({
            "sessionId": "s1",
            "prompt": [
                {"type": "text", "text": "first"},
                {"type": "image", "data": "..."},
                {"type": "text", "text": "second"},
            ],
        });
        assert_eq!(extract_prompt_text(&params), "first\n\nsecond");
    }

    #[test]
    fn extract_prompt_text_empty_when_no_prompt_field() {
        assert_eq!(extract_prompt_text(&json!({})), "");
    }

    #[test]
    fn respond_shapes_a_jsonrpc_result_envelope() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        respond(&tx, Some(json!(7)), json!({"sessionId": "abc"}));
        let msgs = recv_all(&mut rx);
        assert_eq!(
            msgs,
            vec![json!({"jsonrpc": "2.0", "id": 7, "result": {"sessionId": "abc"}})]
        );
    }

    #[test]
    fn respond_sends_nothing_for_a_notification_with_no_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        respond(&tx, None, json!({"ignored": true}));
        assert!(recv_all(&mut rx).is_empty());
    }

    #[test]
    fn respond_error_shapes_a_jsonrpc_error_envelope() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        respond_error(&tx, Some(json!("req-1")), -32601, "method not found: foo");
        let msgs = recv_all(&mut rx);
        assert_eq!(
            msgs,
            vec![json!({
                "jsonrpc": "2.0",
                "id": "req-1",
                "error": {"code": -32601, "message": "method not found: foo"},
            })]
        );
    }

    #[tokio::test]
    async fn handle_method_initialize_advertises_protocol_version_and_agent_info() {
        let mut state = SessionState {
            agent: test_agent(),
            session_id: String::new(),
            run: None,
            pending_prompt_id: None,
            session_histories: HashMap::new(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let session_id_slot = Arc::new(Mutex::new(String::new()));
        handle_method(
            "initialize",
            Some(json!(1)),
            json!({"protocolVersion": 1}),
            &mut state,
            &tx,
            &session_id_slot,
        )
        .await;
        let msgs = recv_all(&mut rx);
        assert_eq!(msgs.len(), 1);
        let result = &msgs[0]["result"];
        assert_eq!(result["protocolVersion"], json!(1));
        assert_eq!(result["agentInfo"]["name"], json!("pirs"));
    }

    #[tokio::test]
    async fn handle_method_session_new_returns_a_session_id_and_stores_it() {
        let mut state = SessionState {
            agent: test_agent(),
            session_id: String::new(),
            run: None,
            pending_prompt_id: None,
            session_histories: HashMap::new(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let session_id_slot = Arc::new(Mutex::new(String::new()));
        handle_method(
            "session/new",
            Some(json!(2)),
            json!({"cwd": "/tmp", "mcpServers": []}),
            &mut state,
            &tx,
            &session_id_slot,
        )
        .await;
        let msgs = recv_all(&mut rx);
        assert_eq!(msgs.len(), 1);
        let session_id = msgs[0]["result"]["sessionId"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty());
        assert_eq!(*session_id_slot.lock().unwrap(), session_id);
        assert_eq!(state.session_id, session_id);
    }

    #[tokio::test]
    async fn handle_method_unknown_method_errors_with_method_not_found() {
        let mut state = SessionState {
            agent: test_agent(),
            session_id: String::new(),
            run: None,
            pending_prompt_id: None,
            session_histories: HashMap::new(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let session_id_slot = Arc::new(Mutex::new(String::new()));
        handle_method(
            "session/does_not_exist",
            Some(json!(3)),
            Value::Null,
            &mut state,
            &tx,
            &session_id_slot,
        )
        .await;
        let msgs = recv_all(&mut rx);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["error"]["code"], json!(-32601));
    }

    fn test_agent() -> Agent {
        struct DummyProvider;
        #[async_trait::async_trait]
        impl pirs_ai::LlmProvider for DummyProvider {
            async fn stream(
                &self,
                _model: &str,
                _context: &pirs_ai::Context,
                _options: &CompletionOptions,
                _cancel: tokio_util::sync::CancellationToken,
            ) -> futures::stream::BoxStream<'static, pirs_ai::StreamEvent> {
                Box::pin(futures::stream::empty())
            }
        }
        Agent::new(Arc::new(DummyProvider), "test-model")
    }

    #[test]
    fn emit_session_update_sends_only_the_new_text_delta_each_time() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let last_text_len = Mutex::new(0usize);

        emit_session_update(
            &tx,
            "sess-1",
            &last_text_len,
            AgentEvent::MessageStart {
                message: Box::new(Message::Assistant(pirs_ai::AssistantMessage::default())),
            },
        );

        let mut msg = pirs_ai::AssistantMessage {
            content: vec![pirs_ai::ContentBlock::text("Hello")],
            ..Default::default()
        };
        emit_session_update(
            &tx,
            "sess-1",
            &last_text_len,
            AgentEvent::MessageUpdate {
                message: Box::new(msg.clone()),
            },
        );
        msg.content = vec![pirs_ai::ContentBlock::text("Hello, world")];
        emit_session_update(
            &tx,
            "sess-1",
            &last_text_len,
            AgentEvent::MessageUpdate {
                message: Box::new(msg),
            },
        );

        let msgs = recv_all(&mut rx);
        assert_eq!(msgs.len(), 2, "MessageStart itself sends nothing: {msgs:?}");
        assert_eq!(msgs[0]["method"], json!("session/update"));
        assert_eq!(msgs[0]["params"]["sessionId"], json!("sess-1"));
        assert_eq!(
            msgs[0]["params"]["update"]["sessionUpdate"],
            json!("agent_message_chunk")
        );
        assert_eq!(
            msgs[0]["params"]["update"]["content"]["type"],
            json!("text")
        );
        assert_eq!(
            msgs[0]["params"]["update"]["content"]["text"],
            json!("Hello")
        );
        assert_eq!(
            msgs[1]["params"]["update"]["content"]["text"],
            json!(", world"),
            "second update should send only the delta, not the full text again"
        );
    }

    #[test]
    fn emit_session_update_tool_call_start_and_end_shapes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let last_text_len = Mutex::new(0usize);

        emit_session_update(
            &tx,
            "sess-1",
            &last_text_len,
            AgentEvent::ToolExecutionStart {
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                args: json!({"command": "ls"}),
            },
        );
        emit_session_update(
            &tx,
            "sess-1",
            &last_text_len,
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                result: Box::new(pirs_ai::ToolResultMessage {
                    tool_call_id: "call-1".into(),
                    tool_name: "bash".into(),
                    content: vec![pirs_ai::ContentBlock::text("total 0")],
                    details: None,
                    is_error: false,
                    terminate: false,
                    timestamp: 0,
                }),
            },
        );

        let msgs = recv_all(&mut rx);
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs[0]["params"]["update"]["sessionUpdate"],
            json!("tool_call")
        );
        assert_eq!(msgs[0]["params"]["update"]["toolCallId"], json!("call-1"));
        assert_eq!(msgs[0]["params"]["update"]["status"], json!("in_progress"));
        assert_eq!(
            msgs[1]["params"]["update"]["sessionUpdate"],
            json!("tool_call_update")
        );
        assert_eq!(msgs[1]["params"]["update"]["status"], json!("completed"));
        assert_eq!(msgs[1]["params"]["update"]["rawOutput"], json!("total 0"));
    }

    #[test]
    fn agent_turn_spawned_off_stdin_task() {
        let src = include_str!("acp_mode.rs");
        assert!(
            src.contains("tokio::spawn(async move") && src.contains("done_tx.send"),
            "agent turn must run on a separate task so permission recv cannot starve stdin"
        );
        assert!(
            src.contains("live_permission_hook"),
            "ACP must install the live permission ladder like interactive modes"
        );
        assert!(
            src.contains("oneshot::channel") || src.contains("oneshot::Receiver"),
            "run wait must be a channel, not a future polled on the stdin select task"
        );
    }

    #[test]
    fn permission_hook_allows_when_client_selects_allow() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let next_id = Arc::new(AtomicU64::new(1));
        let session_id_slot = Arc::new(Mutex::new("sess-1".to_string()));
        let hook = acp_permission_hook(
            Arc::clone(&pending),
            out_tx,
            Arc::clone(&next_id),
            Arc::clone(&session_id_slot),
        );

        let handle =
            std::thread::spawn(move || hook("call-1", "bash", &json!({"command": "rm x"})));

        // Simulate the reader loop: wait for the outbound request, then
        // resolve it as if the client answered "allow".
        let request = loop {
            if let Ok(v) = out_rx.try_recv() {
                break v;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(request["method"], json!("session/request_permission"));
        assert_eq!(request["params"]["sessionId"], json!("sess-1"));
        assert_eq!(request["params"]["toolCall"]["toolCallId"], json!("call-1"));
        let req_id = request["id"].as_u64().unwrap();

        let sender = pending.lock().unwrap().remove(&req_id).unwrap();
        sender
            .send(json!({"outcome": {"outcome": "selected", "optionId": "allow"}}))
            .unwrap();

        assert_eq!(
            handle.join().unwrap(),
            None,
            "allow should not block the tool call"
        );
    }

    #[test]
    fn permission_hook_denies_when_client_selects_deny() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let next_id = Arc::new(AtomicU64::new(1));
        let session_id_slot = Arc::new(Mutex::new("sess-1".to_string()));
        let hook = acp_permission_hook(pending.clone(), out_tx, next_id, session_id_slot);

        let handle =
            std::thread::spawn(move || hook("call-2", "bash", &json!({"command": "rm x"})));
        let request = loop {
            if let Ok(v) = out_rx.try_recv() {
                break v;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let req_id = request["id"].as_u64().unwrap();
        let sender = pending.lock().unwrap().remove(&req_id).unwrap();
        sender
            .send(json!({"outcome": {"outcome": "selected", "optionId": "deny"}}))
            .unwrap();

        assert!(
            handle.join().unwrap().is_some(),
            "deny should block the tool call"
        );
    }

    #[test]
    fn permission_hook_denies_when_client_cancels() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let next_id = Arc::new(AtomicU64::new(1));
        let session_id_slot = Arc::new(Mutex::new("sess-1".to_string()));
        let hook = acp_permission_hook(pending.clone(), out_tx, next_id, session_id_slot);

        let handle = std::thread::spawn(move || hook("call-3", "bash", &json!({})));
        let request = loop {
            if let Ok(v) = out_rx.try_recv() {
                break v;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let req_id = request["id"].as_u64().unwrap();
        let sender = pending.lock().unwrap().remove(&req_id).unwrap();
        sender
            .send(json!({"outcome": {"outcome": "cancelled"}}))
            .unwrap();

        assert!(handle.join().unwrap().is_some());
    }

    #[test]
    fn permission_hook_fails_closed_with_no_active_session() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel();
        let next_id = Arc::new(AtomicU64::new(1));
        let session_id_slot = Arc::new(Mutex::new(String::new()));
        let hook = acp_permission_hook(pending, out_tx, next_id, session_id_slot);
        assert!(hook("call-4", "bash", &json!({})).is_some());
    }
}
