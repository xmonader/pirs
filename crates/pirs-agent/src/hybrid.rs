//! Agentpy-style hybrid: weak driver + strong advisor, harness-owned escalation.
//!
//! When a strategy sets `hybrid: true` (built-in `weak-drive`) and the run has
//! a `--plan-model`, full-scope executor phases get:
//!
//! 1. **Thrash → advisor** — loop/mistake thrash injects strong-model guidance
//!    and *continues* the weak loop (instead of a hard stop).
//! 2. **Staged escalate** — summary → full trajectory → (optional) stop after
//!    budget; takeover is left to the next strategy phase (review/fixup).
//! 3. **`ask_advisor` tool** — weak model can request guidance; never trusted
//!    for "am I stuck?" (that's thrash's job).
//!
//! Scheduled plan + review still live in the phase list of `weak-drive.rhai`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use pirs_ai::{
    CompletionOptions, Context, LlmProvider, Message, StreamEvent,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, ToolExecContext, ToolOutput};

const ADVISOR_SYSTEM: &str = "You are advising a smaller coding model that is working on a \
task. You cannot act — you can only give guidance. Be concrete and specific: \
name files, functions, and concrete next steps. Be brief. If the approach it has \
taken is wrong, say so plainly and describe the correct approach instead.";

/// Shared state for one hybrid run (one strategy attempt).
#[derive(Clone)]
pub struct HybridConfig {
    inner: Arc<HybridInner>,
}

struct HybridInner {
    provider: Arc<dyn LlmProvider>,
    /// Strong / plan-model id used for advisor calls.
    strong_model: String,
    api_key: Option<String>,
    /// Original issue / task text for advisor context.
    task: Mutex<String>,
    /// Plan text from the strong planning phase (filled after plan completes).
    plan: Mutex<String>,
    stage: AtomicUsize,
    max_advisor_calls: usize,
    advisor_calls: AtomicUsize,
    /// Recent thrash reasons for logging / stage 2 context.
    last_reasons: Mutex<Vec<String>>,
}

impl HybridConfig {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        strong_model: impl Into<String>,
        api_key: Option<String>,
        task: impl Into<String>,
        max_advisor_calls: usize,
    ) -> Self {
        Self {
            inner: Arc::new(HybridInner {
                provider,
                strong_model: strong_model.into(),
                api_key,
                task: Mutex::new(task.into()),
                plan: Mutex::new(String::new()),
                stage: AtomicUsize::new(0),
                max_advisor_calls: max_advisor_calls.max(1),
                advisor_calls: AtomicUsize::new(0),
                last_reasons: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn strong_model(&self) -> &str {
        &self.inner.strong_model
    }

    pub fn set_plan(&self, plan: impl Into<String>) {
        *self.inner.plan.lock().unwrap() = plan.into();
    }

    pub fn plan(&self) -> String {
        self.inner.plan.lock().unwrap().clone()
    }

    pub fn advisor_calls(&self) -> usize {
        self.inner.advisor_calls.load(Ordering::Relaxed)
    }

    /// Called when thrash fires mid-loop. Returns guidance to inject and continue,
    /// or `None` to hard-stop (budget exhausted / stages done).
    pub async fn on_thrash(
        &self,
        thrash_msg: &str,
        recent_messages: &[Message],
    ) -> Option<String> {
        let calls = self.inner.advisor_calls.load(Ordering::Relaxed);
        if calls >= self.inner.max_advisor_calls {
            eprintln!(
                "[hybrid] thrash escalate skipped — advisor budget exhausted ({calls}/{})",
                self.inner.max_advisor_calls
            );
            return None;
        }

        let stage = self.inner.stage.fetch_add(1, Ordering::Relaxed) + 1;
        self.inner
            .last_reasons
            .lock()
            .unwrap()
            .push(thrash_msg.chars().take(200).collect());

        let task = self.inner.task.lock().unwrap().clone();
        let plan = self.inner.plan.lock().unwrap().clone();
        let tail = message_tail(recent_messages, if stage == 1 { 6 } else { 40 });

        let context = if stage == 1 {
            format!(
                "Detected thrash (stage 1 — short summary):\n{thrash_msg}\n\n\
                 Original plan:\n{}\n\n\
                 Recent activity:\n{tail}",
                if plan.is_empty() { "(none)" } else { &plan }
            )
        } else {
            format!(
                "Detected thrash (stage {stage} — fuller trajectory):\n{thrash_msg}\n\n\
                 Original plan:\n{}\n\n\
                 Trajectory:\n{tail}",
                if plan.is_empty() { "(none)" } else { &plan }
            )
        };

        let question = format!(
            "The weaker coding agent appears stuck on this task:\n{task}\n\n\
             What should it do next? Be concrete."
        );

        match self.advise(&question, &context).await {
            Ok(guidance) => {
                eprintln!(
                    "[hybrid] thrash escalate stage={stage} advisor_calls={}",
                    self.advisor_calls()
                );
                Some(format!(
                    "[ADVISOR — strong model reviewed thrash: {thrash_msg}]\n{guidance}"
                ))
            }
            Err(e) => {
                eprintln!("[hybrid] advisor call failed: {e:#}");
                // Fall through to hard stop if we cannot advise.
                None
            }
        }
    }

    /// Free-form advisor call (tool path).
    pub async fn advise(&self, question: &str, context: &str) -> anyhow::Result<String> {
        let calls = self.inner.advisor_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if calls > self.inner.max_advisor_calls {
            // Undo the bump conceptually — still refuse.
            return Ok(
                "Advisor budget exhausted. Continue on your own with the tools you have."
                    .into(),
            );
        }

        let user = format!("Question:\n{question}\n\nContext:\n{context}");
        let opts = CompletionOptions {
            api_key: self.inner.api_key.clone(),
            max_tokens: Some(1024),
            // Keep advisor cheap when the backend supports it.
            ..Default::default()
        };
        let ctx = Context {
            system_prompt: Some(ADVISOR_SYSTEM.into()),
            messages: vec![Message::user(user)],
            tools: Vec::new(),
        };
        let text = complete_text(
            Arc::clone(&self.inner.provider),
            &self.inner.strong_model,
            &ctx,
            &opts,
        )
        .await?;
        Ok(text)
    }
}

/// Collect a non-tool completion into a single string.
async fn complete_text(
    provider: Arc<dyn LlmProvider>,
    model: &str,
    ctx: &Context,
    opts: &CompletionOptions,
) -> anyhow::Result<String> {
    let stream = provider
        .stream(model, ctx, opts, CancellationToken::new())
        .await;
    let mut text = String::new();
    let mut stream = std::pin::pin!(stream);
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(d) => text.push_str(&d),
            StreamEvent::Done(msg) => {
                let t = msg.text();
                if !t.is_empty() {
                    text = t;
                }
                break;
            }
            StreamEvent::Error(message) => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    Ok(text.trim().to_string())
}

fn user_text(u: &pirs_ai::UserMessage) -> String {
    match &u.content {
        pirs_ai::UserContent::Text(t) => t.clone(),
        pirs_ai::UserContent::Blocks(bs) => bs
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn message_tail(messages: &[Message], n: usize) -> String {
    let start = messages.len().saturating_sub(n);
    let mut out = String::new();
    for m in &messages[start..] {
        let line = match m {
            Message::User(u) => {
                format!(
                    "[user] {}",
                    user_text(u).chars().take(600).collect::<String>()
                )
            }
            Message::Assistant(a) => {
                format!(
                    "[assistant] {}",
                    a.text().chars().take(600).collect::<String>()
                )
            }
            Message::ToolResult(r) => {
                format!(
                    "[tool:{}] {}",
                    r.tool_name,
                    r.model_text().chars().take(400).collect::<String>()
                )
            }
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Tool the weak model can call for strategic help (never for stuck detection).
pub struct AskAdvisorTool {
    hybrid: HybridConfig,
}

impl AskAdvisorTool {
    pub fn new(hybrid: HybridConfig) -> Self {
        Self { hybrid }
    }
}

#[async_trait::async_trait]
impl AgentTool for AskAdvisorTool {
    fn name(&self) -> &str {
        "ask_advisor"
    }

    fn description(&self) -> &str {
        "Consult a stronger model for strategic guidance when stuck, choosing \
         between approaches, or planning multi-step work. Prefer concrete \
         questions and a short summary of what you have tried."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The specific question for the stronger model."
                },
                "context_summary": {
                    "type": "string",
                    "description": "Brief summary of what you have tried and what is failing."
                }
            },
            "required": ["question", "context_summary"]
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let question = ctx
            .args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let summary = ctx
            .args
            .get("context_summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let plan = self.hybrid.plan();
        let context = if plan.is_empty() {
            summary
        } else {
            format!("Original plan:\n{plan}\n\nWhat the agent tried:\n{summary}")
        };
        let guidance = self.hybrid.advise(&question, &context).await?;
        Ok(ToolOutput::text(guidance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::{AssistantMessage, ContentBlock, StopReason, Usage};

    struct FakeProvider {
        reply: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FakeProvider {
        async fn stream(
            &self,
            _model: &str,
            _context: &Context,
            _options: &CompletionOptions,
            _cancel: CancellationToken,
        ) -> futures::stream::BoxStream<'static, StreamEvent> {
            let reply = self.reply.clone();
            Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta(reply.clone()),
                StreamEvent::Done(Box::new(AssistantMessage {
                    content: vec![ContentBlock::text(reply)],
                    stop_reason: StopReason::Stop,
                    usage: Usage::default(),
                    ..Default::default()
                })),
            ]))
        }
    }

    #[tokio::test]
    async fn thrash_escalates_with_guidance() {
        let h = HybridConfig::new(
            Arc::new(FakeProvider {
                reply: "Edit foo.rs line 12".into(),
            }),
            "strong",
            None,
            "fix the bug",
            4,
        );
        h.set_plan("1. fix foo.rs");
        let g = h
            .on_thrash("loop detection: tool `read` repeated", &[])
            .await
            .expect("guidance");
        assert!(g.contains("ADVISOR"));
        assert!(g.contains("foo.rs"));
        assert_eq!(h.advisor_calls(), 1);
    }

    #[tokio::test]
    async fn budget_exhaustion_stops_escalate() {
        let h = HybridConfig::new(
            Arc::new(FakeProvider {
                reply: "ok".into(),
            }),
            "strong",
            None,
            "task",
            1,
        );
        assert!(h.on_thrash("thrash1", &[]).await.is_some());
        // Second thrash: budget used up by first advise.
        assert!(h.on_thrash("thrash2", &[]).await.is_none());
    }
}
