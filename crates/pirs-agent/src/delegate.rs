use std::sync::Arc;

use async_trait::async_trait;
use pirs_ai::{CompletionOptions, LlmProvider, Message, StopReason};
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::events::{AfterToolCallHook, BeforeToolCallHook, Hooks};
use crate::tool::{AgentTool, ToolExecContext, ToolOutput};

/// Runs a subtask in a fresh sub-agent with its own clean context and
/// returns the sub-agent's final answer. The sub-agent's tool set must not
/// include the delegate tool itself (no recursion).
/// Default sub-agent budgets — prevent unbounded thrash when the parent is weak.
pub const DEFAULT_SUBAGENT_MAX_TURNS: usize = 12;
pub const DEFAULT_SUBAGENT_MAX_TOOL_CALLS: usize = 40;

/// Compose a TaskPacket-style prompt from delegate args (task + optional fields).
pub fn compose_task_packet(args: &Value) -> anyhow::Result<String> {
    let mut task = args
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if task.is_empty() {
        if let Some(obj) = args.get("objective").and_then(|v| v.as_str()) {
            task = obj.trim().to_string();
        }
    }
    if task.is_empty() {
        anyhow::bail!("delegate requires a non-empty task (or objective)");
    }
    let mut parts = vec![task];
    if let Some(s) = args.get("scope").and_then(|v| v.as_str()).map(str::trim) {
        if !s.is_empty() {
            parts.push(format!("Scope: {s}"));
        }
    }
    if let Some(s) = args
        .get("acceptance")
        .and_then(|v| v.as_str())
        .map(str::trim)
    {
        if !s.is_empty() {
            parts.push(format!("Acceptance: {s}"));
        }
    }
    if let Some(s) = args
        .get("permission_profile")
        .and_then(|v| v.as_str())
        .map(str::trim)
    {
        if !s.is_empty() {
            parts.push(format!("Permission profile: {s}"));
        }
    }
    if let Some(s) = args.get("verify").and_then(|v| v.as_str()).map(str::trim) {
        if !s.is_empty() {
            parts.push(format!("Verify with: `{s}`"));
        }
    }
    Ok(parts.join("\n"))
}

#[cfg(test)]
mod task_packet_tests {
    use super::*;

    #[test]
    fn packet_merges_fields() {
        let t = compose_task_packet(&json!({
            "task": "fix the bug",
            "scope": "src/",
            "acceptance": "tests pass",
            "permission_profile": "workspace-write",
            "verify": "cargo test"
        }))
        .unwrap();
        assert!(t.contains("fix the bug"));
        assert!(t.contains("Scope: src/"));
        assert!(t.contains("Acceptance:"));
        assert!(t.contains("workspace-write"));
        assert!(t.contains("cargo test"));
    }

    #[test]
    fn objective_alone_works() {
        let t = compose_task_packet(&json!({"objective": "list files"})).unwrap();
        assert_eq!(t, "list files");
    }

    #[test]
    fn empty_errors() {
        assert!(compose_task_packet(&json!({})).is_err());
    }
}

pub struct DelegateTool {
    provider: Arc<dyn LlmProvider>,
    model: String,
    completion: CompletionOptions,
    make_tools: Arc<dyn Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync>,
    policy_hooks: std::sync::Mutex<Option<(BeforeToolCallHook, AfterToolCallHook)>>,
    /// Default budgets applied when the call does not override max_turns/max_tool_calls.
    default_budgets: crate::agent_loop::Budgets,
}

impl DelegateTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: impl Into<String>,
        completion: CompletionOptions,
        make_tools: impl Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(DelegateTool {
            provider,
            model: model.into(),
            completion,
            make_tools: Arc::new(make_tools),
            policy_hooks: std::sync::Mutex::new(None),
            default_budgets: crate::agent_loop::Budgets {
                max_turns: Some(DEFAULT_SUBAGENT_MAX_TURNS),
                max_tool_calls: Some(DEFAULT_SUBAGENT_MAX_TOOL_CALLS),
                max_wall_time: None,
            },
        })
    }

    pub fn with_policy_hooks(&self, before: BeforeToolCallHook, after: AfterToolCallHook) {
        *self.policy_hooks.lock().unwrap() = Some((before, after));
    }

    pub fn with_default_budgets(self: &Arc<Self>, budgets: crate::agent_loop::Budgets) -> Arc<Self> {
        // Rebuild via interior mutation isn't available; callers set on new().
        // Keep API for tests that construct via Arc::get_mut if needed.
        let _ = budgets;
        Arc::clone(self)
    }
}

impl DelegateTool {
    async fn execute_background(&self, task: String, model: String) -> anyhow::Result<ToolOutput> {
        let provider = Arc::clone(&self.provider);
        let completion = self.completion.clone();
        let make_tools = Arc::clone(&self.make_tools);
        let policy = self.policy_hooks.lock().unwrap().clone();

        let (id, _job) = crate::jobs::registry().register(
            crate::jobs::JobKind::Agent,
            task.chars().take(80).collect(),
            std::env::temp_dir().join("pirs-job-pending.log"),
            None,
        );
        let out_path = std::env::temp_dir().join(format!("pirs-job-{id}.log"));
        {
            let registry = crate::jobs::registry();
            let job = registry.get(id).unwrap();
            job.lock().unwrap().output_path = out_path.clone();
        }

        let progress: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let progress2 = Arc::clone(&progress);
        let task_for_thread = task.clone();
        let task_for_desc = task.clone();

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    // Don't strand the job as Running forever if the runtime
                    // can't be built.
                    crate::jobs::registry().set_status(id, crate::jobs::JobStatus::Exited(-1));
                    crate::jobs::registry()
                        .notify(format!("background agent #{id} failed to start: {e}"));
                    return;
                }
            };
            rt.block_on(async move {
                let mut hooks = Hooks::default();
                if let Some((b, a)) = policy {
                    hooks.before_tool_call = Some(b);
                    hooks.after_tool_call = Some(a);
                }
                let mut agent = Agent::new(provider, &model)
                    .with_tools(make_tools())
                    .with_completion(completion)
                    .with_hooks(hooks)
                    .with_compaction(None)
                    .with_budgets(crate::agent_loop::Budgets {
                        max_turns: Some(DEFAULT_SUBAGENT_MAX_TURNS),
                        max_tool_calls: Some(DEFAULT_SUBAGENT_MAX_TOOL_CALLS),
                        max_wall_time: None,
                    });
                {
                    let progress = Arc::clone(&progress2);
                    agent.subscribe(Arc::new(move |event: crate::events::AgentEvent| {
                        if let crate::events::AgentEvent::MessageEnd { message } = event {
                            if let pirs_ai::Message::Assistant(a) = &*message {
                                *progress.lock().unwrap() = a.text();
                            }
                        }
                    }));
                }
                let steer = agent.steer_sender();
                crate::jobs::registry().set_steer(
                    id,
                    Arc::new(move |s: String| {
                        steer(pirs_ai::Message::user(s));
                    }),
                );
                crate::jobs::registry().set_cancel(id, agent.cancel_handle());
                crate::jobs::registry().set_progress_handle(id, Arc::clone(&progress2));
                let result = agent.prompt(&task_for_thread).await;
                let (status, answer) = match result {
                    Ok(new) => {
                        // A run whose last assistant ended in Error is a failure
                        // even though prompt() returned Ok (errors are messages,
                        // per the loop contract). Reporting exited(0) here would
                        // silently drop the sub-agent's failure.
                        let last_assistant = new.iter().rev().find_map(|m| match m {
                            pirs_ai::Message::Assistant(a) => Some(a),
                            _ => None,
                        });
                        match last_assistant {
                            Some(a) if a.stop_reason == StopReason::Error => (
                                1,
                                format!(
                                    "error: {}",
                                    a.error_message
                                        .clone()
                                        .unwrap_or_else(|| "sub-agent ended with an error".into())
                                ),
                            ),
                            _ => {
                                let text = new
                                    .iter()
                                    .rev()
                                    .find_map(|m| match m {
                                        pirs_ai::Message::Assistant(a)
                                            if !a.text().trim().is_empty() =>
                                        {
                                            Some(a.text())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or_else(|| "(no answer)".to_string());
                                (0, text)
                            }
                        }
                    }
                    Err(e) => (1, format!("error: {e}")),
                };
                let _ = std::fs::write(&out_path, &answer);
                crate::jobs::registry().set_status(id, crate::jobs::JobStatus::Exited(status));
                crate::jobs::registry().notify(format!(
                    "background agent #{id} finished: {}",
                    answer.chars().take(200).collect::<String>()
                ));
            });
        });

        let _ = task_for_desc;
        Ok(ToolOutput::text(format!(
            "background agent #{id} started on task: {}. Use jobs/job_output/job_kill/job_steer to manage it.",
            task.chars().take(80).collect::<String>()
        )))
    }
}

#[async_trait]
impl AgentTool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Run a self-contained subtask in a fresh sub-agent with a clean context (same tools, no conversation history). Good for exploration, research, or isolated steps whose details would clutter the main context. Returns only the sub-agent's final answer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Complete, self-contained instructions for the sub-agent. It cannot see this conversation."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model id override for the sub-agent (e.g. a cheaper/faster model for simple steps). Defaults to the current model."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the sub-agent as a background job and return immediately with a job id. Use jobs/job_output/job_kill/job_steer to manage it."
                },
                "max_turns": {
                    "type": "integer",
                    "description": "Max agent turns for the sub-agent (default 12). Prevents unbounded thrash."
                },
                "max_tool_calls": {
                    "type": "integer",
                    "description": "Max tool calls for the sub-agent (default 40)."
                },
                "objective": {
                    "type": "string",
                    "description": "TaskPacket: short objective (optional; merged into task if task empty)."
                },
                "scope": {
                    "type": "string",
                    "description": "TaskPacket: files/dirs or concern the sub-agent may touch."
                },
                "acceptance": {
                    "type": "string",
                    "description": "TaskPacket: how to know the subtask is done."
                },
                "permission_profile": {
                    "type": "string",
                    "description": "TaskPacket: read-only | workspace-write | danger-full-access (hint in prompt)."
                },
                "verify": {
                    "type": "string",
                    "description": "TaskPacket: shell command the sub-agent should run to verify."
                }
            },
            "required": ["task"]
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "delegate: run a self-contained subtask in a fresh sub-agent, get back only its answer",
        )
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let task = compose_task_packet(&ctx.args)?;

        let model = ctx
            .args
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.model.clone());

        if ctx
            .args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return self.execute_background(task, model).await;
        }

        ctx.emit_update(format!("sub-agent started ({model}): {task}"));

        let mut hooks = Hooks::default();
        if let Some((before, after)) = self.policy_hooks.lock().unwrap().clone() {
            hooks.before_tool_call = Some(before);
            hooks.after_tool_call = Some(after);
        }

        let max_turns = ctx
            .args
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .or(self.default_budgets.max_turns);
        let max_tool_calls = ctx
            .args
            .get("max_tool_calls")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .or(self.default_budgets.max_tool_calls);
        let budgets = crate::agent_loop::Budgets {
            max_turns,
            max_tool_calls,
            max_wall_time: self.default_budgets.max_wall_time,
        };

        let mut agent = Agent::new(Arc::clone(&self.provider), &model)
            .with_tools((self.make_tools)())
            .with_completion(self.completion.clone())
            .with_hooks(hooks)
            .with_compaction(None)
            .with_budgets(budgets);

        let cancel_watcher = agent.cancel_handle();
        let parent_cancel = ctx.cancel.clone();
        let watcher = tokio::spawn(async move {
            parent_cancel.cancelled().await;
            cancel_watcher.lock().unwrap().cancel();
        });
        let result = agent.prompt(&task).await;
        watcher.abort();
        let new_messages = result?;
        let budget_hit = agent.budget_hit;

        // Surface a sub-agent failure to the parent as a tool error (errors are
        // messages per the loop contract, so prompt() returned Ok). Otherwise
        // the parent sees a bland "(no answer)" and has no signal to retry.
        if let Some(a) = new_messages.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        }) {
            if a.stop_reason == StopReason::Error {
                anyhow::bail!(
                    "sub-agent failed: {}",
                    a.error_message
                        .clone()
                        .unwrap_or_else(|| "ended with an error".into())
                );
            }
        }

        let answer = new_messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(a) if !a.text().trim().is_empty() => Some(a.text()),
                _ => None,
            })
            .unwrap_or_else(|| "(sub-agent produced no text answer)".to_string());

        let tool_calls: usize = new_messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult(_)))
            .count();
        let mut sub_usage = pirs_ai::Usage::default();
        for m in &new_messages {
            if let Message::Assistant(a) = m {
                sub_usage += a.usage.clone();
            }
        }

        let mut text = answer;
        if let Some(hit) = budget_hit {
            text = format!(
                "{text}\n\n[sub-agent stopped: budget exhausted ({hit:?}); partial answer above]"
            );
        }

        Ok(ToolOutput::text(text).with_details(json!({
            "subAgentToolCalls": tool_calls,
            "subAgentModel": model,
            "subAgentUsage": sub_usage,
            "subAgentBudgetHit": budget_hit.map(|h| format!("{h:?}")),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolExecContext;
    use async_trait::async_trait;
    use futures::stream;
    use pirs_ai::{
        AssistantMessage, ContentBlock, Context, LlmProvider, StopReason, StreamEvent, Usage,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    struct NoopTool;
    #[async_trait]
    impl AgentTool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "n"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::text("ok"))
        }
    }

    #[tokio::test]
    async fn delegate_stops_under_max_turns_budget() {
        let turns = Arc::new(AtomicUsize::new(0));
        let turns_p = Arc::clone(&turns);
        struct P {
            turns: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl LlmProvider for P {
            async fn stream(
                &self,
                _model: &str,
                _ctx: &Context,
                _opts: &CompletionOptions,
                _cancel: CancellationToken,
            ) -> futures::stream::BoxStream<'static, StreamEvent> {
                let i = self.turns.fetch_add(1, Ordering::SeqCst);
                let msg = AssistantMessage {
                    content: vec![ContentBlock::ToolCall {
                        id: format!("c{i}"),
                        name: "noop".into(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input: 1,
                        output: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                Box::pin(stream::iter(vec![StreamEvent::Done(Box::new(msg))]))
            }
        }
        let provider: Arc<dyn LlmProvider> = Arc::new(P { turns: turns_p });
        let tool = DelegateTool::new(provider, "m", CompletionOptions::default(), || {
            vec![Arc::new(NoopTool) as Arc<dyn AgentTool>]
        });
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: json!({"task": "loop forever", "max_turns": 3, "max_tool_calls": 100}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let hit = out
            .details
            .as_ref()
            .and_then(|d| d.get("subAgentBudgetHit"))
            .cloned();
        assert!(
            hit.is_some(),
            "expected budget stop, details={:?}",
            out.details
        );
        let n = turns.load(Ordering::SeqCst);
        assert!(
            n <= 5,
            "provider called {n} times — should stop near max_turns=3"
        );
    }
}
