//! Last-resort test runner for when no static detector confirms a runner at
//! all (e.g. Django's or sympy's custom test invocation, which this harness's
//! rhai detectors don't yet recognize). Instead of failing the instance
//! outright with `RunnerUndetected`, hand the specific test ids to a bounded,
//! edit-free sub-agent: investigate the repo (docs, CI config, trial
//! commands) and self-report pass/fail for each id.
//!
//! **Trust note — read before using.** Every other [`TestRunner`] in this
//! harness gets its verdict from independently parsing a real test run's
//! output (JUnit XML); the whole benchmark's value rests on that
//! independence ("the harness judges, the agent's own 'I'm done' is only
//! advisory"). This runner is the one deliberate exception: its
//! [`Snapshot`] is the discovery sub-agent's own self-report, not something
//! the harness re-verified. It only ever activates as the last resort — when
//! no static detector confirms anything at all — and every id it reports is
//! tagged in the trace so a reader can tell a self-reported outcome apart
//! from a harness-confirmed one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::agent_loop::{run_agent_loop, Budgets, LoopConfig};
use pirs_agent::{AgentTool, Emit, ExecutionMode, ToolExecContext, ToolOutput};
use pirs_ai::{CompletionOptions, Context, LlmProvider, Message};
use pirs_bench::{Ring, Snapshot, TestId, TestOutcome, TestRunner};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Tools the discovery sub-agent may use. It investigates and *runs* things
/// (so it needs a shell), but must never edit the tree it's judging — that
/// stays the fix executor's job, never this runner's.
const DISCOVERY_MUTATING: &[&str] = &["edit", "write", "ast_edit"];

/// One id's self-reported outcome, as the discovery agent phrases it.
#[derive(Debug, Clone, Deserialize)]
struct ReportedResult {
    id: String,
    outcome: String, // "pass" | "fail" | "error" | "unknown"
}

/// Captures the discovery agent's one required tool call and ends its turn.
/// This is the *only* way the sub-agent can conclude — there is no other
/// termination path, so a run that never calls it exhausts its turn budget
/// and every id defaults to [`TestOutcome::NotCollected`] (never a silent
/// pass).
struct ReportResultsTool {
    slot: Arc<Mutex<Option<Vec<ReportedResult>>>>,
}

#[async_trait]
impl AgentTool for ReportResultsTool {
    fn name(&self) -> &str {
        "report_test_results"
    }

    fn description(&self) -> &str {
        "Report your final findings: the current pass/fail outcome for every \
         requested test id, based on tests you actually ran — never a guess. \
         Call this exactly once, when done investigating. Include every id \
         you were given; any omitted id counts as not verified."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "outcome": {
                                "type": "string",
                                "enum": ["pass", "fail", "error", "unknown"]
                            }
                        },
                        "required": ["id", "outcome"]
                    }
                }
            },
            "required": ["results"]
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let results: Vec<ReportedResult> = ctx
            .args
            .get("results")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let n = results.len();
        *self.slot.lock().unwrap() = Some(results);
        Ok(ToolOutput::text(format!("recorded {n} result(s)")).terminate())
    }
}

/// Renders the discovery sub-agent's system prompt for one batch of ids.
fn discovery_system_prompt(ids: &[TestId]) -> String {
    format!(
        "You are investigating how to run specific tests in a real repository. \
         This project's test framework was NOT recognized by the harness's \
         built-in detectors — it likely uses a custom test runner rather than \
         a standard one. Your job is ONLY to determine whether each listed \
         test id currently passes or fails right now. Do not edit any files.\n\n\
         You may read documentation (README, CONTRIBUTING, docs/, CI config) \
         and run shell commands to investigate and actually execute the \
         tests. Only report an outcome you have verified by running it — \
         never guess. When you have a real answer for every id, call \
         report_test_results with one entry per id.\n\n\
         Test ids:\n{}",
        ids.join("\n")
    )
}

/// A cheap, best-effort fingerprint of the working tree's current state:
/// HEAD's sha plus the raw working-tree diff against it. Equal fingerprints
/// mean "nothing has changed since the last investigation" — falls back to a
/// constant when git isn't available (e.g. not a repo), which degrades to
/// "treat every call in this runner's lifetime as the same state" rather than
/// failing outright.
fn tree_fingerprint(work_dir: &std::path::Path) -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(work_dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    match (run(&["rev-parse", "HEAD"]), run(&["diff"])) {
        (Some(head), Some(diff)) => format!("{head}\0{diff}"),
        _ => "no-git".to_string(),
    }
}

/// A [`TestRunner`] backed by a bounded, edit-free sub-agent instead of a
/// deterministic subprocess. See the module doc for the trust trade-off this
/// makes — last-resort, only for instances no static detector covers at all.
///
/// **Caches its self-report per tree state** (see [`tree_fingerprint`]).
/// Necessary, not just an optimization: [`crate::baseline`]'s stability check
/// calls `.run()` twice expecting the SAME answer before trusting a baseline
/// at all, and a fresh independent LLM investigation each time is neither
/// deterministic nor guaranteed to agree with itself — two honest-but-
/// differently-worded investigations of an unchanged tree would otherwise
/// look "flaky" and abort every instance with `BaselineUnusable`. Caching by
/// tree state also gives the right behavior for the opposite case: once a fix
/// attempt actually edits the tree, the fingerprint changes and a fresh
/// investigation runs, so the post-fix verify pass reflects the real new
/// state rather than a stale pre-fix answer.
pub struct AgentDiscoveredRunner {
    rt: Arc<tokio::runtime::Runtime>,
    provider: Arc<dyn LlmProvider>,
    model: String,
    api_key: String,
    tools: Vec<Arc<dyn AgentTool>>,
    max_turns: usize,
    work_dir: PathBuf,
    /// (tree fingerprint, exact requested ids, self-reported snapshot).
    /// Keyed on the exact id set too, not just tree state — the driver calls
    /// `.run()` with different scopes across a run (inner ring vs. the wider
    /// regression ring), and reusing one scope's snapshot for another would
    /// silently under/over-report coverage. A linear scan is fine: at most a
    /// handful of distinct (fingerprint, ids) pairs exist per instance.
    cache: Mutex<Vec<(String, Vec<TestId>, Snapshot)>>,
}

impl AgentDiscoveredRunner {
    pub fn new(
        rt: Arc<tokio::runtime::Runtime>,
        provider: Arc<dyn LlmProvider>,
        model: String,
        api_key: String,
        work_dir: PathBuf,
        max_turns: usize,
    ) -> Self {
        let mut tools = pirs_tools::default_tools(work_dir.clone());
        tools.retain(|t| !DISCOVERY_MUTATING.contains(&t.name()));
        AgentDiscoveredRunner {
            rt,
            provider,
            model,
            api_key,
            tools,
            max_turns,
            work_dir,
            cache: Mutex::new(Vec::new()),
        }
    }
}

impl TestRunner for AgentDiscoveredRunner {
    fn run(&self, ids: &[TestId], _ring: Ring) -> anyhow::Result<Snapshot> {
        let fingerprint = tree_fingerprint(&self.work_dir);
        {
            let cache = self.cache.lock().unwrap();
            if let Some((_, _, snap)) = cache
                .iter()
                .find(|(fp, cached_ids, _)| fp == &fingerprint && cached_ids.as_slice() == ids)
            {
                return Ok(snap.clone());
            }
        }

        let slot: Arc<Mutex<Option<Vec<ReportedResult>>>> = Arc::new(Mutex::new(None));
        let mut tools = self.tools.clone();
        tools.push(Arc::new(ReportResultsTool { slot: slot.clone() }));

        let mut ctx = Context {
            system_prompt: Some(discovery_system_prompt(ids)),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let cfg = LoopConfig {
            model: self.model.clone(),
            completion: CompletionOptions {
                api_key: Some(self.api_key.clone()),
                max_tokens: Some(8_000),
                ..Default::default()
            },
            tool_execution: ExecutionMode::Parallel,
            hooks: Default::default(),
            compaction: None,
            visible_tools: None,
            extra_usage: Arc::new(Mutex::new(pirs_ai::Usage::default())),
            cascade: None,
            budgets: Budgets {
                max_turns: Some(self.max_turns),
                max_tool_calls: None,
                max_wall_time: None,
            },
            thrash: None,
            skip_remaining_if: None,
        };
        let emit: Emit = Arc::new(|_| {});
        let prompt = "Investigate and report the current pass/fail state of every listed test id."
            .to_string();

        self.rt.block_on(run_agent_loop(
            vec![Message::user(prompt)],
            &mut ctx,
            &tools,
            &self.provider,
            &cfg,
            &emit,
            CancellationToken::new(),
        ));

        let reported = slot.lock().unwrap().take().unwrap_or_default();
        tracing::warn!(
            "agent-discovered runner self-report for {} id(s): {:?}",
            ids.len(),
            reported
        );
        let pairs = ids.iter().map(|id| {
            let outcome = reported
                .iter()
                .find(|r| &r.id == id)
                .map(|r| match r.outcome.as_str() {
                    "pass" => TestOutcome::Pass,
                    "fail" | "error" => TestOutcome::Fail,
                    _ => TestOutcome::NotCollected,
                })
                .unwrap_or(TestOutcome::NotCollected);
            if outcome == TestOutcome::NotCollected {
                tracing::warn!("id never reported by the discovery agent: {id}");
            }
            (id.clone(), outcome)
        });
        let snap = Snapshot::from_pairs(pairs);
        self.cache
            .lock()
            .unwrap()
            .push((fingerprint, ids.to_vec(), snap.clone()));
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::{AssistantMessage, ContentBlock, StopReason, StreamEvent};

    /// A scripted provider standing in for a real LLM: its one scripted turn
    /// emits the tool call the discovery agent needs; the loop then
    /// terminates because that tool sets `terminate: true`. Exercises the
    /// real tool-dispatch and Snapshot-construction path without a network
    /// call.
    struct ScriptedProvider;

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn stream(
            &self,
            model: &str,
            _context: &Context,
            _options: &CompletionOptions,
            _cancel: CancellationToken,
        ) -> futures::stream::BoxStream<'static, StreamEvent> {
            let msg = AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "1".into(),
                    name: "report_test_results".into(),
                    arguments: json!({
                        "results": [
                            {"id": "m::pass_id", "outcome": "pass"},
                            {"id": "m::fail_id", "outcome": "fail"},
                        ]
                    }),
                    thought_signature: None,
                }],
                stop_reason: StopReason::ToolUse,
                model: model.to_string(),
                ..Default::default()
            };
            Box::pin(futures::stream::iter(vec![
                StreamEvent::Start,
                StreamEvent::Done(Box::new(msg)),
            ]))
        }
    }

    #[test]
    fn self_reported_outcomes_land_in_the_snapshot_and_omitted_ids_are_not_collected() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider);
        let runner = AgentDiscoveredRunner::new(
            rt,
            provider,
            "scripted".into(),
            "k".into(),
            dir.path().to_path_buf(),
            5,
        );
        let ids = vec![
            "m::pass_id".to_string(),
            "m::fail_id".to_string(),
            "m::never_mentioned".to_string(),
        ];
        let snap = runner.run(&ids, Ring::Inner).unwrap();
        assert_eq!(snap.get("m::pass_id"), Some(TestOutcome::Pass));
        assert_eq!(snap.get("m::fail_id"), Some(TestOutcome::Fail));
        // An id the agent never reported must never silently count as a pass.
        assert_eq!(
            snap.get("m::never_mentioned"),
            Some(TestOutcome::NotCollected)
        );
    }

    /// Scripted responses in order; each call to `.stream()` pops the next one
    /// and bumps a shared counter — lets a test assert exactly how many times
    /// a fresh investigation actually ran, distinguishing that from a cache
    /// hit that never calls the provider at all.
    struct CountingProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        scripted: Mutex<std::collections::VecDeque<&'static str>>,
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        async fn stream(
            &self,
            model: &str,
            _context: &Context,
            _options: &CompletionOptions,
            _cancel: CancellationToken,
        ) -> futures::stream::BoxStream<'static, StreamEvent> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = self
                .scripted
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or("unknown");
            let msg = AssistantMessage {
                content: vec![ContentBlock::ToolCall {
                    id: "1".into(),
                    name: "report_test_results".into(),
                    arguments: json!({"results": [{"id": "m::t", "outcome": outcome}]}),
                    thought_signature: None,
                }],
                stop_reason: StopReason::ToolUse,
                model: model.to_string(),
                ..Default::default()
            };
            Box::pin(futures::stream::iter(vec![
                StreamEvent::Start,
                StreamEvent::Done(Box::new(msg)),
            ]))
        }
    }

    fn git_init(dir: &std::path::Path) {
        let sh = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
        };
        sh(&["init", "-q"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "init"]);
    }

    #[test]
    fn same_tree_state_reuses_the_cached_self_report_without_a_second_investigation() {
        // This is the actual bug this cache exists to fix: baseline capture
        // calls .run() twice expecting the SAME answer to confirm stability.
        // Two independent, non-deterministic LLM investigations of an
        // unchanged tree are not guaranteed to agree with themselves — which
        // showed up live as every agent-discovered instance failing with
        // BaselineUnusable. Caching by tree state means the second call never
        // re-investigates at all.
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(CountingProvider {
            calls: calls.clone(),
            scripted: Mutex::new(std::collections::VecDeque::from(["fail", "pass"])),
        });
        let runner = AgentDiscoveredRunner::new(
            rt,
            provider,
            "scripted".into(),
            "k".into(),
            dir.path().to_path_buf(),
            5,
        );
        let ids = vec!["m::t".to_string()];

        let first = runner.run(&ids, Ring::Inner).unwrap();
        let second = runner.run(&ids, Ring::Inner).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(first.get("m::t"), Some(TestOutcome::Fail));
        assert_eq!(second.get("m::t"), first.get("m::t"));

        // A real edit changes the tree's fingerprint — the next call must
        // re-investigate (the whole point: post-fix verify must reflect the
        // NEW state, never a stale pre-fix answer).
        std::fs::write(dir.path().join("f.txt"), "v2\n").unwrap();
        let third = runner.run(&ids, Ring::Inner).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(third.get("m::t"), Some(TestOutcome::Pass));
    }
}
