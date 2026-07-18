use std::sync::Arc;

use async_trait::async_trait;
use pirs_ai::{CompletionOptions, LlmProvider, Message};
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::events::{AfterToolCallHook, BeforeToolCallHook, Hooks};
use crate::tool::{AgentTool, ToolExecContext, ToolOutput};

/// Runs a subtask in a fresh sub-agent with its own clean context and
/// returns the sub-agent's final answer. The sub-agent's tool set must not
/// include the delegate tool itself (no recursion).
pub struct DelegateTool {
    provider: Arc<dyn LlmProvider>,
    model: String,
    completion: CompletionOptions,
    make_tools: Arc<dyn Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync>,
    policy_hooks: std::sync::Mutex<Option<(BeforeToolCallHook, AfterToolCallHook)>>,
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
        })
    }

    pub fn with_policy_hooks(&self, before: BeforeToolCallHook, after: AfterToolCallHook) {
        *self.policy_hooks.lock().unwrap() = Some((before, after));
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
                Err(_) => return,
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
                    .with_compaction(None);
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
                crate::jobs::registry().set_progress_handle(id, Arc::clone(&progress2));
                let result = agent.prompt(&task_for_thread).await;
                let (status, answer) = match result {
                    Ok(new) => {
                        let text = new
                            .iter()
                            .rev()
                            .find_map(|m| match m {
                                pirs_ai::Message::Assistant(a) if !a.text().trim().is_empty() => {
                                    Some(a.text())
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| "(no answer)".to_string());
                        (0, text)
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
        let task = ctx
            .args
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if task.is_empty() {
            anyhow::bail!("delegate requires a non-empty task");
        }

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

        let mut agent = Agent::new(Arc::clone(&self.provider), &model)
            .with_tools((self.make_tools)())
            .with_completion(self.completion.clone())
            .with_hooks(hooks)
            .with_compaction(None);

        let new_messages = agent.prompt(&task).await?;

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

        Ok(ToolOutput::text(answer).with_details(json!({
            "subAgentToolCalls": tool_calls,
            "subAgentModel": model,
            "subAgentUsage": sub_usage,
        })))
    }
}
