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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pirs_agent::agent_loop::{run_agent_loop, Budgets, LoopConfig};
use pirs_agent::{AgentTool, Emit, ExecutionMode, Hooks};
use pirs_ai::anthropic::AnthropicClient;
use pirs_ai::{CompletionOptions, Context, LlmProvider, Message, Usage};
use pirs_bench::{Executor, GitWorkspace, Verdict};
use pirs_graph::ast_edit::AstEditTool;
use pirs_graph::code_map::CodeMapTool;
use pirs_graph::LazyGraph;
use tokio_util::sync::CancellationToken;

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
    /// Watches the tree so we can tell whether an attempt actually changed code.
    ws: GitWorkspace,
    issue: String,
    targets: Vec<String>,
    started: bool,
}

impl AgentExecutor {
    /// Build an executor rooted at `repo_root`. `issue` is the task/problem
    /// statement; `targets` are the failing test ids being fixed (used only to
    /// tell the agent what to make pass — the harness owns actual verification).
    pub fn new(
        repo_root: PathBuf,
        issue: String,
        targets: Vec<String>,
        model: String,
        api_key: String,
        max_turns_per_attempt: usize,
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

        let provider: Arc<dyn LlmProvider> = Arc::new(AnthropicClient::new(None));

        let context = Context {
            system_prompt: Some(SYSTEM_PROMPT.to_string()),
            messages: Vec::new(),
            tools: Vec::new(),
        };

        Ok(AgentExecutor {
            rt,
            provider,
            tools,
            context,
            model,
            api_key,
            max_turns_per_attempt,
            ws: GitWorkspace::new(repo_root),
            issue,
            targets,
            started: false,
        })
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
        let emit: Emit = Arc::new(|_ev| {});
        let cancel = CancellationToken::new();
        let provider = Arc::clone(&self.provider);
        let rt = Arc::clone(&self.rt);

        rt.block_on(async {
            let _ = run_agent_loop(
                vec![Message::user(prompt)],
                &mut self.context,
                &self.tools,
                &provider,
                &config,
                &emit,
                cancel,
            )
            .await;
        });

        let after = self.ws.diff().unwrap_or_default();
        // A candidate worth verifying exists iff the tree changed this attempt.
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
