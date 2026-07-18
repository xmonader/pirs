use std::sync::{Arc, Mutex};

use pirs_agent::agent_loop::CascadeJudge;
use pirs_agent::events::{AfterToolCallHook, BeforeToolCallHook, Hooks};
use pirs_ai::{CompletionOptions, LlmProvider, Message};

pub type PolicySlot = Arc<Mutex<Option<(BeforeToolCallHook, AfterToolCallHook)>>>;
pub type UsageSlot = Arc<Mutex<pirs_ai::Usage>>;

/// Builds the closure rhai's `run_subagent` uses: a fresh agent on a
/// dedicated thread with its own current-thread runtime, returning the last
/// assistant text. Shared by REPL and RPC wiring (was duplicated).
pub fn build_subagent_runner(
    provider: Arc<dyn LlmProvider>,
    completion: CompletionOptions,
    default_model: String,
    tools: Vec<Arc<dyn pirs_agent::AgentTool>>,
    policy_slot: PolicySlot,
    usage_slot: UsageSlot,
) -> pirs_rhai::SubagentRunner {
    Arc::new(move |task: String, model: Option<String>| {
        let provider = Arc::clone(&provider);
        let completion = completion.clone();
        let model = model.unwrap_or_else(|| default_model.clone());
        let policy = policy_slot.lock().unwrap().clone();
        let usage_slot = Arc::clone(&usage_slot);
        let tools = tools.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async move {
                let mut hooks = Hooks::default();
                if let Some((b, a)) = &policy {
                    hooks.before_tool_call = Some(b.clone());
                    hooks.after_tool_call = Some(a.clone());
                }
                let mut agent = pirs_agent::Agent::new(provider, &model)
                    .with_tools(tools)
                    .with_completion(completion)
                    .with_hooks(hooks)
                    .with_compaction(None);
                let new = agent.prompt(&task).await.map_err(|e| e.to_string())?;
                *usage_slot.lock().unwrap() += agent.usage_report().grand_total();
                new.iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::Assistant(a) if !a.text().trim().is_empty() => Some(a.text()),
                        _ => None,
                    })
                    .ok_or_else(|| "sub-agent produced no answer".to_string())
            })
        })
        .join()
        .unwrap_or_else(|_| Err("sub-agent thread panicked".to_string()))
    })
}

/// Builds the cascade judge: a cheap structural check plus an LLM verdict
/// from the draft model. Shared by REPL and RPC wiring.
pub fn build_cascade_judge(provider: Arc<dyn LlmProvider>, judge_model: String) -> CascadeJudge {
    Arc::new(move |draft| {
        let provider = Arc::clone(&provider);
        let model = judge_model.clone();
        let draft_text = draft.text();
        let stop = draft.stop_reason;
        let has_tool_calls = !draft.tool_calls().is_empty();
        Box::pin(async move {
            if matches!(
                stop,
                pirs_ai::StopReason::Error | pirs_ai::StopReason::Aborted
            ) {
                return false;
            }
            if !has_tool_calls && draft_text.trim().is_empty() {
                return false;
            }
            let prompt =
                format!("Rate this agent turn as ACCEPT or REJECT (one word). Turn: {draft_text}");
            let ctx = pirs_ai::Context {
                system_prompt: Some("You are a terse judge. Reply ACCEPT or REJECT.".into()),
                messages: vec![Message::user(prompt)],
                tools: vec![],
            };
            let mut stream = provider
                .stream(
                    &model,
                    &ctx,
                    &Default::default(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await;
            let mut verdict = String::new();
            use futures::StreamExt;
            while let Some(ev) = stream.next().await {
                match ev {
                    pirs_ai::StreamEvent::TextDelta(d) => verdict.push_str(&d),
                    pirs_ai::StreamEvent::Done(m) => {
                        if m.stop_reason == pirs_ai::StopReason::Error {
                            return false;
                        }
                    }
                    _ => {}
                }
            }
            // Verdict must start with ACCEPT to pass; anything else rejects.
            verdict.trim_start().to_uppercase().starts_with("ACCEPT")
        })
    })
}
