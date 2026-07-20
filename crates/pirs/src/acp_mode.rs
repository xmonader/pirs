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
//! Scope of this first cut — a real, working subset, not the full spec:
//!   - Agent-side methods implemented: `initialize`, `session/new`,
//!     `session/prompt`, `session/cancel` (a notification, not a request).
//!   - Client-side methods called: `session/update` (streamed notifications:
//!     `agent_message_chunk` for assistant text, `tool_call`/
//!     `tool_call_update` for tool execution) and `session/request_permission`
//!     (every tool call is gated through the client — there is no local
//!     auto/yolo/ask distinction in ACP mode, the client's human is always
//!     the approver).
//!   - **Not implemented**: `fs/read_text_file`/`fs/write_text_file` (pirs's
//!     tools read/write the real filesystem directly rather than routing
//!     through the client, so an editor's unsaved-buffer content isn't
//!     visible to it — a real limitation, not an oversight), `terminal/*`,
//!     `session/load`, `authenticate`, multiple concurrent sessions (a
//!     second `session/new` replaces the current session rather than
//!     running alongside it — most embedding editors open one agent session
//!     per project/panel anyway, but this is a real scope limit to know
//!     about before relying on it for multi-session use).
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
}

const PROTOCOL_VERSION: u64 = 1;

type RunOutput = (
    Vec<Message>,
    Vec<Message>,
    Option<pirs_agent::agent_loop::BudgetHit>,
);
type RunFuture = std::pin::Pin<Box<dyn std::future::Future<Output = RunOutput> + Send>>;

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
    run: Option<RunFuture>,
    /// The `session/prompt` request id to answer once `run` resolves —
    /// `session/prompt` doesn't respond until the whole turn (including any
    /// steering/tool calls) finishes.
    pending_prompt_id: Option<Value>,
}

pub async fn run(opts: AcpOptions) -> anyhow::Result<()> {
    let cwd = opts.cwd.clone();
    let provider: Arc<dyn pirs_ai::LlmProvider> = if std::env::var("PIRS_PROVIDER").as_deref()
        == Ok("anthropic")
    {
        Arc::new(
            pirs_ai::AnthropicClient::new(opts.base_url.clone()).with_max_retries(opts.max_retries),
        )
    } else {
        Arc::new(
            pirs_ai::OpenAiCompat::new(opts.base_url.clone()).with_max_retries(opts.max_retries),
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
    let mut hooks = Hooks {
        before_tool_call: Some(acp_permission_hook(
            Arc::clone(&pending),
            out_tx.clone(),
            Arc::clone(&next_out_id),
            Arc::clone(&session_id_slot),
        )),
        ..Default::default()
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
    host.load_default_dirs(&cwd);
    for err in &host.load_errors {
        eprintln!("[extension error] {err}");
    }
    let host = Arc::new(host);
    tools.extend(host.tools());
    let ext_hooks = host.hooks();

    if let (Some(b), Some(a)) = (&ext_hooks.before_tool_call, &ext_hooks.after_tool_call) {
        if let Some(chained) =
            pirs_agent::Hooks::chain_before(hooks.before_tool_call.clone(), Some(b.clone()))
        {
            *policy_slot.lock().unwrap() = Some((chained, a.clone()));
        }
    }
    hooks.before_tool_call =
        pirs_agent::Hooks::chain_before(hooks.before_tool_call.clone(), ext_hooks.before_tool_call);
    hooks.after_tool_call = ext_hooks.after_tool_call;
    hooks.transform_context = ext_hooks.transform_context;
    hooks.should_stop_after_turn = ext_hooks.should_stop_after_turn;
    hooks.get_steering_messages = ext_hooks.get_steering_messages;
    hooks.get_follow_up_messages = ext_hooks.get_follow_up_messages;

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
        .with_hooks(hooks)
        .with_compaction(Some(pirs_agent::compaction::CompactionConfig::default()));

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
                    Some(f) => f.await,
                    None => std::future::pending().await,
                }
            } => Cmd::RunFinished(result),
        };

        match cmd {
            Cmd::RunFinished((full, _new, hit)) => {
                state.run = None;
                state.agent.budget_hit = hit;
                state.agent.complete_run(full);
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
            respond(
                out,
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": false,
                        "promptCapabilities": {
                            "image": false,
                            "audio": false,
                            "embeddedContext": false,
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
            let session_id = format!("acp-{}-{}", std::process::id(), pirs_ai::now_millis());
            *session_id_slot.lock().unwrap() = session_id.clone();
            state.session_id = session_id.clone();
            respond(out, id, json!({"sessionId": session_id}));
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
            let text = extract_prompt_text(&params);
            state.pending_prompt_id = id;
            match state.agent.begin_prompt(vec![Message::user(text)]) {
                Ok(fut) => state.run = Some(Box::pin(fut)),
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
        other => {
            if id.is_some() {
                respond_error(out, id, -32601, &format!("method not found: {other}"));
            }
        }
    }
}

fn extract_prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
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

/// Every tool call is gated through the client via `session/request_permission`
/// — this closure is `pirs_agent`'s synchronous `BeforeToolCallHook`, so it
/// blocks the calling task on a `std::sync::mpsc::Receiver` until the stdin
/// reader loop resolves the matching pending outbound request (see `run`'s
/// `Cmd::Incoming` handling for responses with no `method`).
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
