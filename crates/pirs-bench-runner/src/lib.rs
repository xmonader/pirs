//! The agent-backed [`Executor`]: the real pirs coding agent, driven as the
//! "fix" step of the benchmark harness.
//!
//! The whole point of the split is trust. The agent *executes* — it localizes
//! (code-graph tool), edits real files (edit/write/ast-edit), and self-corrects
//! by running commands. The harness *judges* — the pirs-bench gate decides
//! success over a real red→green flip, so the agent's own "I'm done" is only
//! advisory.
//!
//! **Bench isolation is structural:** we assemble the tool set ourselves from
//! `pirs_tools::default_tools` plus the graph tools, and never construct a
//! `pirs_rhai::ExtensionHost`. There is therefore no path by which the task
//! repo's own `.pirs/extensions`, hooks, or MCP config load into this run.

pub mod metrics;
pub mod selftest;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pirs_agent::agent_loop::{run_agent_loop, Budgets, LoopConfig};
use pirs_agent::strategy::{self, PhaseDriver, ToolScope};
use pirs_agent::trace::Recorder;
use pirs_agent::{AgentEvent, AgentTool, Emit, ExecutionMode, Hooks};
use pirs_ai::anthropic::AnthropicClient;
use pirs_ai::{CompletionOptions, Context, LlmProvider, Message, OpenAiCompat, Usage};
use pirs_bench::{Executor, GitWorkspace, Verdict};
use pirs_graph::ast_edit::AstEditTool;
use pirs_graph::code_map::CodeMapTool;
use pirs_graph::LazyGraph;
use tokio_util::sync::CancellationToken;

use crate::metrics::{SessionStats, UsageByModel};

/// The general loop-strategy type lives in `pirs-agent`; re-exported so the bench
/// CLI and any other consumer name it from one place.
pub use pirs_agent::strategy::Strategy;

/// Which LLM backend to drive the agent with.
#[derive(Debug, Clone)]
pub enum Provider {
    /// Anthropic Messages API (default endpoint). Key: `ANTHROPIC_API_KEY`.
    Anthropic,
    /// Any OpenAI-compatible endpoint (DeepSeek, OpenAI, local servers) at
    /// `base_url`. Key: provider-specific env, resolved by the caller.
    OpenAiCompat { base_url: String, name: String },
}

impl Provider {
    /// DeepSeek's OpenAI-compatible endpoint.
    pub fn deepseek() -> Self {
        Provider::OpenAiCompat {
            base_url: "https://api.deepseek.com".to_string(),
            name: "deepseek".to_string(),
        }
    }
}

/// Construct a concrete provider. Anthropic and OpenAI-compatible backends both
/// implement [`LlmProvider`]; the API key travels per-request via
/// `CompletionOptions`, not the constructor.
pub fn build_provider(provider: &Provider) -> Arc<dyn LlmProvider> {
    match provider {
        Provider::Anthropic => Arc::new(AnthropicClient::new(None)),
        Provider::OpenAiCompat { base_url, name } => {
            Arc::new(OpenAiCompat::new(Some(base_url.clone())).with_provider_name(name.clone()))
        }
    }
}

/// Model/provider/budget knobs for an [`AgentExecutor`].
pub struct AgentConfig {
    pub model: String,
    pub api_key: String,
    pub max_turns_per_attempt: usize,
    pub provider: Arc<dyn LlmProvider>,
    /// The loop strategy to drive the fix with (a built-in or a user script).
    pub strategy: Strategy,
    /// Optional flight recorder; when set, every agent event (full messages, tool
    /// args/results, turns) plus phase/attempt boundaries are traced to JSONL.
    pub recorder: Option<Arc<Recorder>>,
}

/// The agent-backed executor and a [`PhaseDriver`]: it holds the assembled tools,
/// the LLM provider, and one [`Context`] per strategy phase. It runs whatever
/// [`Strategy`] it is configured with (built-in or user-authored) on an owned
/// Tokio runtime, and is the bench harness's fix `Executor`.
pub struct AgentExecutor {
    rt: Arc<tokio::runtime::Runtime>,
    provider: Arc<dyn LlmProvider>,
    tools: Vec<Arc<dyn AgentTool>>,
    /// Read-only subset of `tools` (no edit/write/ast_edit/bash) used by any
    /// read-only phase so planning cannot mutate the tree.
    planner_tools: Vec<Arc<dyn AgentTool>>,
    /// The loop strategy (phase list) this executor runs.
    strategy: Strategy,
    /// One conversation per strategy phase, keyed by phase id. Persistent
    /// strategies reuse a phase's context across attempts; split strategies get a
    /// fresh one each attempt (the driver honours the `fresh` flag).
    phase_contexts: HashMap<String, Context>,
    /// Optional flight recorder shared across the whole run.
    recorder: Option<Arc<Recorder>>,
    /// Attempt counter, so trace events can be scoped to the attempt they belong
    /// to (the harness may call `attempt` several times per instance).
    attempt_no: u32,
    model: String,
    api_key: String,
    max_turns_per_attempt: usize,
    /// Watches the tree, extracts diffs, and restores protected test files.
    ws: GitWorkspace,
    issue: String,
    targets: Vec<String>,
    /// Test files (derived from targets + keep_green) restored to base after each
    /// attempt so a fix can never pass by editing the tests.
    protected: Vec<String>,
    /// Per-session token usage, keyed by model.
    usage: UsageByModel,
    /// Per-session behavior stats, updated live from the agent event stream.
    stats: Arc<Mutex<SessionStats>>,
}

impl AgentExecutor {
    /// Build an executor rooted at `repo_root`. `issue` is the problem statement;
    /// `targets` are the failing test ids to fix and `keep_green` those that must
    /// stay green — both contribute their test files to the protected set. The
    /// harness, not the agent, owns actual verification.
    pub fn new(
        repo_root: PathBuf,
        issue: String,
        targets: Vec<String>,
        keep_green: Vec<String>,
        config: AgentConfig,
    ) -> anyhow::Result<Self> {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?,
        );

        // Bench-safe tool set: base file/shell/search tools + code graph. No
        // ExtensionHost, so no repo-supplied scripts, hooks, or MCP.
        let mut tools = pirs_tools::default_tools(repo_root.clone());
        let graph = Arc::new(LazyGraph::new(repo_root.clone()));
        tools.push(Arc::new(CodeMapTool::new(graph, repo_root.clone())));
        tools.push(Arc::new(AstEditTool::new(repo_root.clone())));

        // Planner tools: everything that can't mutate the tree, so the plan phase
        // localizes without editing. bash is excluded too (it can write via shell).
        const MUTATING: &[&str] = &["edit", "write", "ast_edit", "bash"];
        let planner_tools: Vec<Arc<dyn AgentTool>> = tools
            .iter()
            .filter(|t| !MUTATING.contains(&t.name()))
            .cloned()
            .collect();

        // The test file of an id is the segment before "::" (pytest/go/nextest
        // node ids all share this shape).
        let protected: Vec<String> = targets
            .iter()
            .chain(keep_green.iter())
            .filter_map(|id| id.split("::").next())
            .filter(|f| !f.is_empty())
            .map(|f| f.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        Ok(AgentExecutor {
            rt,
            provider: config.provider,
            tools,
            planner_tools,
            strategy: config.strategy,
            phase_contexts: HashMap::new(),
            recorder: config.recorder,
            attempt_no: 0,
            model: config.model,
            api_key: config.api_key,
            max_turns_per_attempt: config.max_turns_per_attempt,
            ws: GitWorkspace::new(repo_root),
            issue,
            targets,
            protected,
            usage: UsageByModel::default(),
            stats: Arc::new(Mutex::new(SessionStats::default())),
        })
    }

    /// This session's per-model token usage so far.
    pub fn session_usage(&self) -> UsageByModel {
        self.usage.clone()
    }

    /// This session's observed behavior (turns, tool calls).
    pub fn session_stats(&self) -> SessionStats {
        self.stats.lock().unwrap().clone()
    }

    fn loop_config(&self) -> LoopConfig {
        LoopConfig {
            model: self.model.clone(),
            completion: CompletionOptions {
                api_key: Some(self.api_key.clone()),
                max_tokens: Some(16_000),
                ..Default::default()
            },
            tool_execution: ExecutionMode::Parallel,
            hooks: Hooks::default(),
            compaction: None,
            visible_tools: None,
            extra_usage: Arc::new(Mutex::new(Usage::default())),
            cascade: None,
            budgets: Budgets {
                max_turns: Some(self.max_turns_per_attempt),
                max_tool_calls: None,
                max_wall_time: None,
            },
        }
    }
}

/// The last non-empty assistant text in a message list — the phase's output
/// (a plan, or a critic's vetted plan).
fn last_assistant_text(msgs: &[Message]) -> String {
    msgs.iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => {
                let t = a.text();
                (!t.trim().is_empty()).then_some(t)
            }
            _ => None,
        })
        .unwrap_or_default()
}

impl AgentExecutor {
    /// Fold each assistant message's token usage into the session totals.
    fn fold_usage(&mut self, msgs: &[Message]) {
        for msg in msgs {
            if let Message::Assistant(a) = msg {
                self.usage.add(&a.model, &a.usage);
            }
        }
    }

    /// Run one agent loop over `context` with `tools`, seeded by `prompt`. Builds
    /// the event hook (behavior stats + per-tool timing), runs to the turn budget,
    /// and returns the raw messages produced. Token usage is folded by the caller
    /// (via [`fold_usage`]) so this stays a plain associated fn and sidesteps
    /// borrowing `self` while a `self`-owned context is passed in.
    #[allow(clippy::too_many_arguments)]
    fn run_loop(
        rt: &Arc<tokio::runtime::Runtime>,
        provider: &Arc<dyn LlmProvider>,
        cfg: &LoopConfig,
        stats: &Arc<Mutex<SessionStats>>,
        tools: &[Arc<dyn AgentTool>],
        context: &mut Context,
        prompt: String,
        recorder: Option<&Arc<Recorder>>,
        phase_id: &str,
    ) -> Vec<Message> {
        // Live behavior stats + per-tool wall-clock, correlated by tool_call_id.
        let stats = Arc::clone(stats);
        let pending: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let rec = recorder.cloned();
        let phase = phase_id.to_string();
        let emit: Emit = Arc::new(move |ev| {
            // Full-fidelity trace: capture EVERY event verbatim (messages, tool
            // args/results, turn boundaries) before deriving the running stats.
            if let Some(r) = &rec {
                r.agent_event(&phase, &ev);
            }
            match ev {
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    ..
                } => {
                    stats.lock().unwrap().record_tool(&tool_name);
                    pending.lock().unwrap().insert(tool_call_id, Instant::now());
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name,
                    ..
                } => {
                    if let Some(start) = pending.lock().unwrap().remove(&tool_call_id) {
                        stats
                            .lock()
                            .unwrap()
                            .add_tool_time(&tool_name, start.elapsed());
                    }
                }
                AgentEvent::TurnEnd { .. } => {
                    stats.lock().unwrap().turns += 1;
                }
                _ => {}
            }
        });
        let cancel = CancellationToken::new();
        rt.block_on(async {
            let (msgs, _budget) = run_agent_loop(
                vec![Message::user(prompt)],
                context,
                tools,
                provider,
                cfg,
                &emit,
                cancel,
            )
            .await;
            msgs
        })
    }
}

impl PhaseDriver for AgentExecutor {
    /// Run one strategy phase: pick the scoped tool set, get or (re)create the
    /// phase's context, drive the loop, fold usage, and return the phase's text.
    fn run_phase(
        &mut self,
        phase_id: &str,
        system: &str,
        prompt: &str,
        scope: ToolScope,
        fresh: bool,
        model: Option<&str>,
    ) -> anyhow::Result<String> {
        // A fresh phase, or one never seen, starts from a clean context holding
        // only its system prompt — the plan/execute split's core mechanic.
        if fresh || !self.phase_contexts.contains_key(phase_id) {
            self.phase_contexts.insert(
                phase_id.to_string(),
                Context {
                    system_prompt: Some(system.to_string()),
                    messages: Vec::new(),
                    tools: Vec::new(),
                },
            );
        }

        // The phase may override the run's default model — the Oracle lever.
        let phase_model = model.unwrap_or(&self.model).to_string();

        if let Some(r) = &self.recorder {
            r.event(
                "phase.start",
                serde_json::json!({
                    "phase": phase_id,
                    "attempt": self.attempt_no,
                    "scope": format!("{scope:?}"),
                    "fresh": fresh,
                    "model": phase_model,
                    "prompt": prompt,
                }),
            );
        }

        let mut cfg = self.loop_config();
        cfg.model = phase_model;
        let tools: &[Arc<dyn AgentTool>] = match scope {
            ToolScope::ReadOnly => &self.planner_tools,
            ToolScope::Full => &self.tools,
        };
        let ctx = self
            .phase_contexts
            .get_mut(phase_id)
            .expect("context inserted above");
        let msgs = Self::run_loop(
            &self.rt,
            &self.provider,
            &cfg,
            &self.stats,
            tools,
            ctx,
            prompt.to_string(),
            self.recorder.as_ref(),
            phase_id,
        );
        self.fold_usage(&msgs);
        let output = last_assistant_text(&msgs);
        if let Some(r) = &self.recorder {
            r.event(
                "phase.end",
                serde_json::json!({
                    "phase": phase_id,
                    "attempt": self.attempt_no,
                    "messages": msgs.len(),
                    "output": output,
                }),
            );
        }
        Ok(output)
    }
}

impl Executor for AgentExecutor {
    fn attempt(&mut self, attempt: u32, last: Option<&Verdict>) -> anyhow::Result<bool> {
        self.attempt_no = attempt;
        if let Some(r) = &self.recorder {
            r.event(
                "attempt.start",
                serde_json::json!({
                    "attempt": attempt,
                    "strategy": self.strategy.name,
                    "prior_verdict": last.map(|v| format!("{v:?}")),
                }),
            );
        }

        // Snapshot the tree so we can tell whether this attempt actually edited
        // anything — if the agent made no change, there is nothing new to verify.
        let before = self.ws.diff().unwrap_or_default();

        // Drive the configured strategy. Cloned so the engine can borrow it while
        // `self` is the mutable driver.
        let strategy = self.strategy.clone();
        let task = strategy::Task {
            issue: self.issue.clone(),
            targets: self.targets.clone(),
            verdict: last.map(|v| format!("{v:?}")),
        };
        strategy::run_strategy(&strategy, self, &task)?;

        // Integrity: revert any edits to protected test files, so verification
        // always runs against the original tests. A fix that only touched a test
        // therefore leaves the tree unchanged and cannot pass.
        let protected: Vec<&str> = self.protected.iter().map(String::as_str).collect();
        let _ = self.ws.restore_paths(&protected);

        let after = self.ws.diff().unwrap_or_default();
        // A candidate worth verifying exists iff the (non-test) tree changed.
        let changed = after != before;
        if let Some(r) = &self.recorder {
            r.event(
                "attempt.end",
                serde_json::json!({ "attempt": attempt, "changed": changed }),
            );
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor(strategy: Strategy) -> AgentExecutor {
        // new() does no I/O beyond spawning a runtime, so a bogus path is fine.
        AgentExecutor::new(
            ".".into(),
            "issue".into(),
            vec!["t.py::test_x".into()],
            vec![],
            AgentConfig {
                model: "m".into(),
                api_key: "k".into(),
                max_turns_per_attempt: 1,
                provider: build_provider(&Provider::Anthropic),
                strategy,
                recorder: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn planner_tools_exclude_mutating_tools() {
        let ex = test_executor(Strategy::plan_exec());
        let planner: Vec<&str> = ex.planner_tools.iter().map(|t| t.name()).collect();
        // Planner can localize but not mutate the tree.
        assert!(planner.contains(&"read"), "planner keeps read: {planner:?}");
        for banned in ["edit", "write", "ast_edit", "bash"] {
            assert!(
                !planner.contains(&banned),
                "planner must exclude {banned}: {planner:?}"
            );
        }
        // The full executor set still has the mutating tools.
        let full: Vec<&str> = ex.tools.iter().map(|t| t.name()).collect();
        assert!(full.contains(&"edit") && full.contains(&"bash"), "{full:?}");
    }

    #[test]
    fn read_only_phase_uses_planner_tools_full_phase_uses_all() {
        // A read-only phase must never expose a mutating tool to the model.
        let ex = test_executor(Strategy::plan_exec());
        let ro: Vec<&str> = match ToolScope::ReadOnly {
            ToolScope::ReadOnly => ex.planner_tools.iter().map(|t| t.name()).collect(),
            ToolScope::Full => ex.tools.iter().map(|t| t.name()).collect(),
        };
        assert!(!ro.contains(&"edit") && !ro.contains(&"write"));
    }
}
