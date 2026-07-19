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

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pirs_agent::agent_loop::{run_agent_loop, Budgets, LoopConfig};
use pirs_agent::{AgentEvent, AgentTool, Emit, ExecutionMode, Hooks};
use pirs_ai::anthropic::AnthropicClient;
use pirs_ai::{CompletionOptions, Context, LlmProvider, Message, OpenAiCompat, Usage};
use pirs_bench::{Executor, GitWorkspace, Verdict};
use pirs_graph::ast_edit::AstEditTool;
use pirs_graph::code_map::CodeMapTool;
use pirs_graph::LazyGraph;
use tokio_util::sync::CancellationToken;

use crate::metrics::{SessionStats, UsageByModel};

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

/// Bench-mode system prompt: fix the code so the failing tests pass, minimally,
/// and never by editing the tests themselves.
const SYSTEM_PROMPT: &str = "\
You are fixing a bug in a real code repository so that specific failing tests pass.

Rules:
- Make the SMALLEST change that makes the failing tests pass. Do not refactor.
- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.
- Use the code-graph and read tools to locate the real cause before editing.
- You may run the project's tests to check your work.
- When the target tests pass and you have not broken others, stop.";

/// Model/provider/budget knobs for an [`AgentExecutor`].
pub struct AgentConfig {
    pub model: String,
    pub api_key: String,
    pub max_turns_per_attempt: usize,
    pub provider: Arc<dyn LlmProvider>,
}

/// The agent-backed executor. Holds the assembled tools, the LLM provider, and a
/// persistent [`Context`] so successive attempts refine cumulatively rather than
/// starting cold. Runs the async agent loop on an owned Tokio runtime.
pub struct AgentExecutor {
    rt: Arc<tokio::runtime::Runtime>,
    provider: Arc<dyn LlmProvider>,
    tools: Vec<Arc<dyn AgentTool>>,
    context: Context,
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
    started: bool,
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

        let context = Context {
            system_prompt: Some(SYSTEM_PROMPT.to_string()),
            messages: Vec::new(),
            tools: Vec::new(),
        };

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
            context,
            model: config.model,
            api_key: config.api_key,
            max_turns_per_attempt: config.max_turns_per_attempt,
            ws: GitWorkspace::new(repo_root),
            issue,
            targets,
            protected,
            started: false,
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

/// The prompt that kicks off the first attempt.
fn initial_prompt(issue: &str, targets: &[String]) -> String {
    let mut p = String::from("Fix the following issue in this repository.\n\n## Issue\n");
    p.push_str(issue.trim());
    p.push_str("\n\n## Tests that must pass after your fix\n");
    for t in targets {
        p.push_str("- ");
        p.push_str(t);
        p.push('\n');
    }
    p.push_str("\nLocate the cause, make the minimal source change, and verify.");
    p
}

/// The nudge after a verification that did not flip the targets. Feeds the gate's
/// verdict back so the agent steers on the concrete failure, not a vague retry.
fn retry_prompt(last: Option<&Verdict>) -> String {
    let detail = match last {
        Some(v) => format!("{v:?}"),
        None => "the target tests still fail".to_string(),
    };
    format!(
        "The target tests are still not passing (gate verdict: {detail}). \
         Re-examine your change against the failing test, find what you missed, \
         and fix the source. Do not modify tests."
    )
}

impl Executor for AgentExecutor {
    fn attempt(&mut self, _attempt: u32, last: Option<&Verdict>) -> anyhow::Result<bool> {
        let prompt = if self.started {
            retry_prompt(last)
        } else {
            self.started = true;
            initial_prompt(&self.issue, &self.targets)
        };

        // Snapshot the tree so we can tell whether this attempt actually edited
        // anything — if the agent made no change, there is nothing new to verify.
        let before = self.ws.diff().unwrap_or_default();

        let config = self.loop_config();
        // Live behavior stats: count turns and tool calls off the event stream so
        // we can tell a real fix from a model that only produced prose.
        let stats = Arc::clone(&self.stats);
        let emit: Emit = Arc::new(move |ev| match ev {
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                stats.lock().unwrap().record_tool(&tool_name);
            }
            AgentEvent::TurnEnd { .. } => {
                stats.lock().unwrap().turns += 1;
            }
            _ => {}
        });
        let cancel = CancellationToken::new();
        let provider = Arc::clone(&self.provider);
        let rt = Arc::clone(&self.rt);

        let new_messages = rt.block_on(async {
            let (msgs, _budget) = run_agent_loop(
                vec![Message::user(prompt)],
                &mut self.context,
                &self.tools,
                &provider,
                &config,
                &emit,
                cancel,
            )
            .await;
            msgs
        });

        // Token accounting: fold each assistant message's usage into this
        // session's per-model totals.
        for msg in &new_messages {
            if let Message::Assistant(a) = msg {
                self.usage.add(&a.model, &a.usage);
            }
        }

        // Integrity: revert any edits the agent made to protected test files, so
        // verification always runs against the original tests. A fix that only
        // touched a test therefore leaves the tree unchanged and cannot pass.
        let protected: Vec<&str> = self.protected.iter().map(String::as_str).collect();
        let _ = self.ws.restore_paths(&protected);

        let after = self.ws.diff().unwrap_or_default();
        // A candidate worth verifying exists iff the (non-test) tree changed.
        Ok(after != before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_prompt_names_issue_and_targets() {
        let p = initial_prompt("The add() function subtracts.", &["test_mymod.py::test_add".into()]);
        assert!(p.contains("The add() function subtracts."));
        assert!(p.contains("test_mymod.py::test_add"));
        assert!(p.contains("minimal"));
    }

    #[test]
    fn retry_prompt_carries_the_verdict_and_forbids_test_edits() {
        let p = retry_prompt(Some(&Verdict::NotYet("t1".into())));
        assert!(p.contains("NotYet"));
        assert!(p.to_lowercase().contains("do not modify tests"));
    }
}
