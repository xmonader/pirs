//! `pirs-bench` — run the pirs coding agent against benchmark instances under the
//! trustworthy verification harness, and emit fixes as patches.
//!
//! Modes:
//! - `solve` — one instance from CLI flags; prints the patch (or `--out`).
//! - `batch` — a JSONL dataset; per-instance patches + attribution histogram.
//! - `selftest` — generate small buggy projects and run the harness over them
//!   (oracle fix by default; `--agent` drives the real model).
//!
//! Each repo is expected to already be checked out at its base commit. The agent
//! edits in place; an accepted outcome yields the unified diff, a failed one
//! rolls the tree back to pristine.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _};
use clap::{Args, Parser, Subcommand, ValueEnum};
use pirs_agent::profile::{Profile, ToolPolicy};
use pirs_agent::trace::Recorder;
use pirs_bench::{
    check_model_patch, is_git_repo, run_instance, Attribution, BaselineCache, DetectorHost,
    Executor, FailBucket, GitWorkspace, Instance, InstanceReport, Outcome, TestRunner, Verdict,
};
use pirs_bench_runner::agent_runner::AgentDiscoveredRunner;
use pirs_bench_runner::{build_provider, selftest, AgentConfig, AgentExecutor, Provider, Strategy};
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(
    name = "pirs-bench",
    about = "Run the pirs agent against benchmark tasks under verification"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Solve a single instance given on the command line.
    Solve(SolveArgs),
    /// Solve every instance in a JSONL dataset and report an attribution histogram.
    Batch(BatchArgs),
    /// Generate small buggy projects and run the harness over them (self-check).
    Selftest(SelftestArgs),
}

/// LLM backend selector.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderKind {
    Anthropic,
    Deepseek,
    /// Any OpenAI-compatible endpoint at `--base-url` (e.g. DashScope,
    /// OpenRouter, a local server). Key: `CUSTOM_API_KEY`.
    OpenaiCompat,
}

/// Knobs shared by all modes.
#[derive(Args, Debug, Clone)]
struct Common {
    /// Model id to drive the agent with (executor / default).
    #[arg(long, default_value = "claude-opus-4-8", global = true)]
    model: String,
    /// Model for read-only planning (and critique) phases only. Executor phases
    /// keep `--model`. Enables strong-plan / weak-exec hybrid economics, e.g.:
    /// `--model qwen3.5-plus --plan-model kimi-k2.5 --strategy plan-exec`.
    /// Both ids must be served by the same `--provider` / `--base-url`.
    #[arg(long, global = true)]
    plan_model: Option<String>,
    /// LLM backend.
    #[arg(long, value_enum, default_value_t = ProviderKind::Anthropic, global = true)]
    provider: ProviderKind,
    /// Base URL for `--provider openai-compat`. Required, and ignored
    /// otherwise.
    #[arg(long, global = true)]
    base_url: Option<String>,
    /// Max verify-gated fix attempts before giving up.
    #[arg(long, default_value_t = 3, global = true)]
    max_attempts: u32,
    /// Max agent turns per attempt (the per-attempt budget).
    #[arg(long, default_value_t = 40, global = true)]
    max_turns: usize,
    /// Agent loop strategy. All are judged identically, so this is the A/B knob.
    #[arg(long, value_enum, default_value_t = StrategyKind::Monolithic, global = true)]
    strategy: StrategyKind,
    /// Path to a user-authored strategy (`.rhai`). Overrides `--strategy`.
    #[arg(long, global = true)]
    strategy_script: Option<PathBuf>,
    /// Bypass the strategy engine entirely: one undivided agent loop with a
    /// generic system prompt, matching the interactive CLI's default (no
    /// `--strategy`/`--profile` given) behavior. Overrides everything below.
    #[arg(long, global = true)]
    no_strategy: bool,
    /// Path to a profile (`.rhai`): a role bundling a strategy with a persona,
    /// model, and tool policy. Overrides `--strategy`/`--strategy-script`.
    #[arg(long, global = true)]
    profile: Option<PathBuf>,
    /// Write a full JSONL event trace (every message, tool call, phase, attempt,
    /// outcome) to this file — the flight recorder for long sessions.
    #[arg(long, global = true)]
    trace: Option<PathBuf>,
    /// Do not list FAIL_TO_PASS target ids in the agent prompt. Harness still
    /// uses `--target` / `--keep-green` for reproduce + verify (fair grading).
    #[arg(long, global = true)]
    hide_targets: bool,
    /// Run the agent only (issue → patch). Skip baseline/reproduce/verify.
    /// Used as phase 1 of strict SWE-bench (`PIRS_STRICT`): no test_patch in tree.
    /// Targets are optional; exit 0 if a non-empty patch is produced.
    #[arg(long, global = true)]
    agent_only: bool,
}

impl Common {
    /// Resolve to a [`Provider`] plus the API key read from the matching env
    /// var (`ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`, or `CUSTOM_API_KEY` for
    /// `--provider openai-compat`, which also requires `--base-url`).
    fn resolve_provider(&self) -> anyhow::Result<(Provider, String)> {
        match self.provider {
            ProviderKind::Anthropic => {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY is not set"))?;
                Ok((Provider::Anthropic, key))
            }
            ProviderKind::Deepseek => {
                let key = std::env::var("DEEPSEEK_API_KEY")
                    .map_err(|_| anyhow::anyhow!("DEEPSEEK_API_KEY is not set"))?;
                Ok((Provider::deepseek(), key))
            }
            ProviderKind::OpenaiCompat => {
                let base_url = self.base_url.clone().ok_or_else(|| {
                    anyhow::anyhow!("--base-url is required for --provider openai-compat")
                })?;
                let key = std::env::var("CUSTOM_API_KEY")
                    .map_err(|_| anyhow::anyhow!("CUSTOM_API_KEY is not set"))?;
                Ok((
                    Provider::OpenAiCompat {
                        base_url,
                        name: "custom".to_string(),
                    },
                    key,
                ))
            }
        }
    }

    /// Load the profile if `--profile` was given.
    fn profile(&self) -> anyhow::Result<Option<Profile>> {
        match &self.profile {
            Some(path) => pirs_rhai::profile_script::load_profile_file(path).map(Some),
            None => Ok(None),
        }
    }

    /// The loop strategy to run. A `--profile` wins (its resolved strategy bakes in
    /// persona + model), then `--strategy-script`, then the selected built-in.
    /// When `--plan-model` is set, read-only phases are pinned to that model.
    fn strategy(&self) -> anyhow::Result<Strategy> {
        if self.no_strategy {
            // No phases at all: AgentConfig.naive (set from this same flag) makes
            // AgentExecutor bypass the strategy engine. This value only names the
            // run in traces/logs.
            return Ok(Strategy {
                name: "none".to_string(),
                steps: Vec::new(),
                persist_across_attempts: true,
                hybrid: false,
            });
        }
        let mut strategy = if let Some(profile) = self.profile()? {
            profile.resolved_strategy()
        } else {
            match &self.strategy_script {
                Some(path) => pirs_rhai::strategy_script::load_strategy_file(path)?,
                None => self.strategy.into(),
            }
        };
        if let Some(pm) = &self.plan_model {
            pirs_agent::strategy::pin_plan_model(&mut strategy, pm);
            eprintln!(
                "plan-model: {pm} (read-only phases) · exec-model: {}",
                self.model
            );
        }
        Ok(strategy)
    }

    /// The tool policy for this run: a profile's `tools` policy, or allow-all.
    fn tool_policy(&self) -> anyhow::Result<ToolPolicy> {
        Ok(self.profile()?.map(|p| p.tools).unwrap_or_default())
    }

    /// Build the flight recorder if `--trace` was given. The run id encodes the
    /// start time and pid so parallel runs never collide.
    fn make_recorder(&self) -> anyhow::Result<Option<Arc<Recorder>>> {
        let Some(path) = &self.trace else {
            return Ok(None);
        };
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let run_id = format!("run-{unix}-{}", std::process::id());
        let rec = Recorder::to_file(path, &run_id, unix)
            .with_context(|| format!("open trace file {path:?}"))?;
        eprintln!("trace: {} -> {path:?}", run_id);
        Ok(Some(rec))
    }
}

/// CLI selector for the agent loop [`Strategy`].
#[derive(Debug, Clone, Copy, ValueEnum)]
enum StrategyKind {
    /// One growing-context loop that localizes, edits, and self-corrects.
    Monolithic,
    /// Read-only planner → fresh executor seeded with only the plan.
    PlanExec,
    /// Planner → critic gate → fresh executor.
    PlanCriticExec,
    /// N read-only planners explore in parallel → merged plan → fresh executor.
    WidePlanExec,
    /// soulrs dual-mode analogue: parallel spark explorers → ember code agent.
    /// CLI aliases: `dual`, `soul-dual` (clap rename below).
    #[value(alias = "dual", alias = "soul-dual", alias = "soulrs-dual")]
    SparkEmber,
    /// Weak executor drives; strong model only at plan + review checkpoints.
    /// CLI aliases: `advisor`, `weak-strong`, `advise-exec`.
    #[value(
        alias = "advisor",
        alias = "weak-strong",
        alias = "advise-exec",
        alias = "weak-advisor"
    )]
    WeakDrive,
}

impl From<StrategyKind> for Strategy {
    fn from(k: StrategyKind) -> Self {
        // Built-ins now live as embedded scripts in pirs-rhai; resolve by name.
        let name = match k {
            StrategyKind::Monolithic => "monolithic",
            StrategyKind::PlanExec => "plan-exec",
            StrategyKind::PlanCriticExec => "plan-critic-exec",
            StrategyKind::WidePlanExec => "wide-plan-exec",
            StrategyKind::SparkEmber => "spark-ember",
            StrategyKind::WeakDrive => "weak-drive",
        };
        pirs_rhai::builtins::builtin(name)
            .unwrap_or_else(|| panic!("built-in strategy {name:?} missing"))
    }
}

#[derive(Args, Debug)]
struct SolveArgs {
    /// Path to the repository, already checked out at the base commit.
    repo: PathBuf,
    /// Failing test id to fix (repeatable). The FAIL_TO_PASS targets.
    /// Optional with `--agent-only` (strict phase 1 has no harness targets).
    #[arg(short = 't', long = "target")]
    targets: Vec<String>,
    /// Test that must stay green (repeatable). The PASS_TO_PASS regression set.
    #[arg(short = 'k', long = "keep-green")]
    keep_green: Vec<String>,
    /// The issue / problem statement text.
    #[arg(short = 'i', long = "issue", conflicts_with = "issue_file")]
    issue: Option<String>,
    /// Read the issue / problem statement from a file.
    #[arg(long = "issue-file", conflicts_with = "issue")]
    issue_file: Option<PathBuf>,
    /// Base commit SHA (for baseline caching). Defaults to the repo's HEAD.
    #[arg(long)]
    base_sha: Option<String>,
    /// Write the resulting patch here instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Don't extract a patch or roll back — just report the outcome.
    #[arg(long)]
    no_patch: bool,
    /// Grade an existing model patch (no agent). Tree must already have
    /// `test_patch` applied so FAIL_TO_PASS can fail at baseline.
    #[arg(long = "check-patch")]
    check_patch: Option<PathBuf>,
    /// Shadow-verify mode: agent works on the **base** tree (never sees this
    /// patch). After each agent attempt, harness grades in a detached git
    /// worktree: apply this test_patch + agent diff, run baseline/verify, and
    /// feed an **opaque** multi-attempt verdict back (no test ids). Isolates
    /// test-visibility while keeping the verify stack (`PIRS_STRICT_VERIFY`).
    #[arg(long = "shadow-test-patch")]
    shadow_test_patch: Option<PathBuf>,
    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug)]
struct BatchArgs {
    /// JSONL file, one instance per line (see `BatchInstance`).
    dataset: PathBuf,
    /// Directory to write per-instance patches into (created if missing).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug)]
struct SelftestArgs {
    /// Directory to generate projects under.
    #[arg(long, default_value = "/tmp/pirstests")]
    dir: PathBuf,
    /// Number of projects to generate and run.
    #[arg(long, default_value_t = 50)]
    count: usize,
    /// Drive the real agent instead of the deterministic oracle fix.
    #[arg(long)]
    agent: bool,
    #[command(flatten)]
    common: Common,
}

/// One line of a batch dataset.
#[derive(Debug, Deserialize)]
struct BatchInstance {
    id: String,
    repo: PathBuf,
    targets: Vec<String>,
    #[serde(default)]
    keep_green: Vec<String>,
    issue: String,
    #[serde(default)]
    base_sha: Option<String>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let result = match cli.cmd {
        Command::Solve(a) => run_solve(a).map(|solved| u8::from(!solved)),
        Command::Batch(a) => run_batch(a).map(|_| 0),
        Command::Selftest(a) => run_selftest(a),
    };
    match result {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("pirs-bench: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// One unit of work for [`solve_one`].
struct Job {
    id: String,
    repo: PathBuf,
    targets: Vec<String>,
    keep_green: Vec<String>,
    issue: String,
    base_sha: Option<String>,
    use_workspace: bool,
}

/// Shared, read-only context for a run.
struct SolveCtx<'a> {
    common: &'a Common,
    provider: &'a Provider,
    api_key: &'a str,
    host: &'a DetectorHost,
    /// The resolved loop strategy (built-in or user script), reused per instance.
    strategy: Strategy,
    /// Tool allow/deny policy (from a profile, or allow-all), reused per instance.
    tool_policy: ToolPolicy,
    /// Optional flight recorder, shared across every instance in the run.
    recorder: Option<Arc<Recorder>>,
}

/// Run one instance through the full harness. Shared by `solve` and `batch`.
fn solve_one(
    job: Job,
    ctx: &SolveCtx,
    cache: &mut BaselineCache,
) -> anyhow::Result<InstanceReport> {
    let repo = job
        .repo
        .canonicalize()
        .with_context(|| format!("repo path {:?}", job.repo))?;

    let workspace = if job.use_workspace && is_git_repo(&repo) {
        Some(GitWorkspace::new(repo.clone()))
    } else {
        None
    };
    let base_sha = match (job.base_sha, &workspace) {
        (Some(s), _) => Some(s),
        (None, Some(ws)) => ws.head_sha().ok(),
        (None, None) => None,
    };

    if let Some(r) = &ctx.recorder {
        r.event(
            "instance.start",
            serde_json::json!({
                "id": job.id,
                "strategy": ctx.strategy.name,
                "model": ctx.common.model,
                "targets": job.targets,
            }),
        );
    }

    let mut executor = AgentExecutor::new(
        repo.clone(),
        job.issue,
        job.targets.clone(),
        job.keep_green.clone(),
        AgentConfig {
            model: ctx.common.model.clone(),
            api_key: ctx.api_key.to_string(),
            max_turns_per_attempt: ctx.common.max_turns,
            provider: build_provider(ctx.provider),
            strategy: ctx.strategy.clone(),
            naive: ctx.common.no_strategy,
            tool_policy: ctx.tool_policy.clone(),
            recorder: ctx.recorder.clone(),
            steering: None,
            hide_targets: ctx.common.hide_targets,
            opaque_verdicts: false,
        },
    )
    .context("build agent executor")?;

    let inst = Instance {
        repo_root: repo.clone(),
        targets: job.targets,
        keep_green: job.keep_green,
        base_sha,
    };

    // run_instance only ever calls this when no static detector confirms a
    // runner at all — see AgentDiscoveredRunner's own doc comment for the
    // trust trade-off this default fallback makes. If detection succeeds
    // normally, this is never invoked and costs nothing.
    let make_fallback = move || -> Box<dyn TestRunner> {
        let rt = Arc::new(
            tokio::runtime::Runtime::new().expect("build tokio runtime for discovery agent"),
        );
        Box::new(AgentDiscoveredRunner::new(
            rt,
            build_provider(ctx.provider),
            ctx.common.model.clone(),
            ctx.api_key.to_string(),
            repo.clone(),
            ctx.common.max_turns,
        ))
    };

    let report = run_instance(
        &inst,
        ctx.host,
        cache,
        &mut executor,
        ctx.common.max_attempts,
        workspace.as_ref(),
        Some(&make_fallback),
    )?;
    if report.used_undetected_fallback {
        eprintln!(
            "[WARNING: no runner detected — outcome is the discovery agent's own \
             self-report, NOT independently verified by the harness]"
        );
    }
    // Surface this session's behavior + token cost.
    let stats = executor.session_stats();
    eprintln!("session: {}", stats.summary());
    eprintln!("{}", executor.session_usage().report());
    // Where every second went: harness phases (discover/bootstrap/baseline/fix/
    // verify/patch) and, within the fix phase, per-tool wall-clock.
    eprintln!("{}", report.timings.report());
    let tool_time = stats.tool_time_summary();
    if !tool_time.is_empty() {
        eprintln!("  fix→tools: {tool_time}");
    }

    // Instance summary into the trace: outcome, tokens, timing, behaviour — so the
    // JSONL is a complete record, not just the fine-grained events.
    if let Some(r) = &ctx.recorder {
        let usage = executor.session_usage().total();
        r.event(
            "instance.end",
            serde_json::json!({
                "id": job.id,
                "outcome": format!("{:?}", report.outcome),
                "accepted": report.outcome.is_accepted(),
                "self_reported_runner": report.used_undetected_fallback,
                "turns": stats.turns,
                "tool_calls": stats.tool_calls,
                "tokens": {
                    "input": usage.input, "output": usage.output,
                    "cache_read": usage.cache_read, "cache_write": usage.cache_write,
                    "reasoning": usage.reasoning, "total": usage.total_tokens,
                },
                "timing_ms": report.timings.total().as_millis() as u64,
            }),
        );
    }
    Ok(report)
}

/// Map a harness [`Outcome`] to a prior [`Verdict`] for multi-attempt feedback.
/// Concrete test ids are never embedded — AgentConfig.opaque_verdicts redacts them.
fn outcome_to_feedback_verdict(o: &Outcome) -> Verdict {
    match o {
        Outcome::Solved | Outcome::AcceptedScopedOnly => Verdict::Done,
        Outcome::Failed(FailBucket::Regressed) => Verdict::Regressed("hidden".into()),
        Outcome::Failed(FailBucket::Flaky) => Verdict::Flaky("hidden".into()),
        // Harness/env noise — not "your fix is wrong". Opaque so the agent
        // re-diagnoses rather than thrashing on a false "still red".
        Outcome::Failed(FailBucket::ReproFailed)
        | Outcome::Failed(FailBucket::BaselineUnusable)
        | Outcome::Failed(FailBucket::RunnerUndetected)
        | Outcome::Failed(FailBucket::EnvSetup) => {
            Verdict::NotYet("harness could not grade this attempt; re-check your source fix".into())
        }
        Outcome::Failed(_) => Verdict::NotYet("required tests still failing".into()),
    }
}

/// `git apply --whitespace=nowarn -` with `patch` on stdin in `dir`.
fn git_apply_stdin(dir: &std::path::Path, patch: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["apply", "--whitespace=nowarn", "-"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn git apply")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch.as_bytes())?;
    }
    let out = child.wait_with_output().context("git apply wait")?;
    if !out.status.success() {
        bail!(
            "git apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Reset repo to `base_sha` (hard) and drop untracked files.
fn git_reset_to(dir: &std::path::Path, base_sha: &str) -> anyhow::Result<()> {
    let st = std::process::Command::new("git")
        .args(["reset", "--hard", base_sha])
        .current_dir(dir)
        .output()
        .context("git reset --hard")?;
    if !st.status.success() {
        bail!(
            "git reset --hard failed: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }
    let st = std::process::Command::new("git")
        .args(["clean", "-fdq"])
        .current_dir(dir)
        .output()
        .context("git clean")?;
    if !st.status.success() {
        bail!("git clean failed: {}", String::from_utf8_lossy(&st.stderr));
    }
    Ok(())
}

/// Apply + commit `test_patch` so HEAD is the red baseline tree.
fn commit_test_patch(dir: &std::path::Path, test_patch: &str) -> anyhow::Result<()> {
    git_apply_stdin(dir, test_patch).context("apply test_patch")?;
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "bench@pirs.local"])
        .current_dir(dir)
        .status();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "pirs-bench"])
        .current_dir(dir)
        .status();
    let _ = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .status();
    let commit = std::process::Command::new("git")
        .args(["commit", "-m", "shadow: apply test_patch"])
        .current_dir(dir)
        .output()
        .context("commit test_patch")?;
    if !commit.status.success() {
        // Empty commit (already applied) is fine; real failures surface later.
        let err = String::from_utf8_lossy(&commit.stderr);
        if !err.contains("nothing to commit") && !err.contains("no changes added") {
            eprintln!("shadow: commit test_patch warning: {err}");
        }
    }
    Ok(())
}

/// Agent on base tree; after each attempt **grade on the same repo** with
/// `test_patch` applied (never left in the tree for the next agent turn).
///
/// We deliberately do **not** use a detached worktree under `/tmp`: SWE-bench
/// images install the package editable from `/testbed` (e.g. Django via
/// `.pth`). Grading in a side worktree still imports `/testbed` source, so a
/// partial agent fix on `/testbed` makes FAIL_TO_PASS look green at baseline
/// → spurious `ReproFailed` in ~2s. Save model patch → reset → test_patch →
/// grade → restore model patch for the next attempt instead.
fn run_shadow_verify_solve(a: SolveArgs) -> anyhow::Result<bool> {
    let shadow_path = a
        .shadow_test_patch
        .as_ref()
        .expect("shadow path checked by caller");
    if a.targets.is_empty() {
        bail!("--shadow-test-patch requires at least one --target for grading");
    }
    let test_patch = std::fs::read_to_string(shadow_path)
        .with_context(|| format!("read shadow test patch {shadow_path:?}"))?;

    let strategy = a.common.strategy()?;
    eprintln!("strategy: {}", strategy.name);
    eprintln!(
        "mode: shadow-verify (agent on base; grade via reset+test_patch on same repo; opaque verdicts)"
    );
    let (provider, key) = a.common.resolve_provider()?;
    let issue = match (a.issue, a.issue_file) {
        (Some(s), _) => s,
        (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("read {f:?}"))?,
        (None, None) => bail!("provide --issue or --issue-file"),
    };
    let repo = a
        .repo
        .canonicalize()
        .with_context(|| format!("repo path {:?}", a.repo))?;
    if !is_git_repo(&repo) {
        bail!("shadow-verify requires a git repo at {repo:?}");
    }
    let ws = GitWorkspace::new(repo.clone());
    let base_sha = match a.base_sha {
        Some(s) => s,
        None => ws.head_sha().context("resolve base sha")?,
    };

    let host = DetectorHost::with_bundled().context("load detectors")?;
    let mut executor = AgentExecutor::new(
        repo.clone(),
        issue,
        a.targets.clone(),
        a.keep_green.clone(),
        AgentConfig {
            model: a.common.model.clone(),
            api_key: key,
            max_turns_per_attempt: a.common.max_turns,
            provider: build_provider(&provider),
            strategy,
            naive: a.common.no_strategy,
            tool_policy: a.common.tool_policy()?,
            recorder: a.common.make_recorder()?,
            steering: None,
            hide_targets: true,
            opaque_verdicts: true,
        },
    )
    .context("build agent executor")?;

    let mut last: Option<Verdict> = None;
    let mut best_patch = String::new();
    let mut accepted = false;

    for attempt in 1..=a.common.max_attempts {
        eprintln!("shadow-verify attempt {attempt}/{}", a.common.max_attempts);
        // Ensure agent always sees base (no test_patch) before editing.
        git_reset_to(&repo, &base_sha).context("reset to base before agent attempt")?;
        if !best_patch.trim().is_empty() && attempt > 1 {
            // Restore prior model patch so the agent iterates, not restarts cold.
            if let Err(e) = git_apply_stdin(&repo, &best_patch) {
                eprintln!("shadow: re-apply prior patch failed ({e}); agent continues from base");
            }
        }

        if !executor.attempt(attempt, last.as_ref())? {
            // No new edits this turn — still allow grade of best_patch if any.
            if best_patch.trim().is_empty() {
                eprintln!("agent produced no further edits; stopping");
                break;
            }
            eprintln!("agent produced no further edits; re-grading best patch");
        } else {
            let model_patch = ws.diff().unwrap_or_default();
            if model_patch.trim().is_empty() {
                eprintln!("empty patch after attempt {attempt}");
                last = Some(Verdict::NotYet("no source changes".into()));
                continue;
            }
            best_patch = model_patch;
        }

        // Grade on the real package root: base → test_patch → model_patch.
        git_reset_to(&repo, &base_sha).context("reset before shadow grade")?;
        if let Err(e) = commit_test_patch(&repo, &test_patch) {
            eprintln!("shadow: test_patch setup failed: {e:#}");
            last = Some(Verdict::NotYet(
                "harness could not apply hidden tests".into(),
            ));
            continue;
        }

        let inst = Instance {
            repo_root: repo.clone(),
            targets: a.targets.clone(),
            keep_green: a.keep_green.clone(),
            base_sha: None, // baseline is current HEAD (post test_patch)
        };
        let report = check_model_patch(&inst, &host, &best_patch)?;
        eprintln!(
            "shadow attempt {attempt}: outcome={:?} timing={}",
            report.outcome,
            report.timings.report().lines().next().unwrap_or("")
        );

        if report.outcome.is_accepted() {
            accepted = true;
            break;
        }
        last = Some(outcome_to_feedback_verdict(&report.outcome));
    }

    let stats = executor.session_stats();
    eprintln!("session: {}", stats.summary());
    eprintln!("{}", executor.session_usage().report());

    // Leave agent tree clean; deliverable is the base-relative patch.
    let _ = git_reset_to(&repo, &base_sha);
    if accepted && !best_patch.trim().is_empty() {
        match &a.out {
            Some(path) => {
                std::fs::write(path, &best_patch)
                    .with_context(|| format!("write patch to {path:?}"))?;
                eprintln!("patch written to {path:?} ({} bytes)", best_patch.len());
            }
            None => println!("{best_patch}"),
        }
        eprintln!("outcome: Solved (shadow-verify)");
        Ok(true)
    } else if !best_patch.trim().is_empty() {
        // Still emit best effort patch for analysis.
        if let Some(path) = &a.out {
            let _ = std::fs::write(path, &best_patch);
            eprintln!(
                "patch written (ungraded/failed) to {path:?} ({} bytes)",
                best_patch.len()
            );
        }
        eprintln!("outcome: Failed(shadow-verify)");
        Ok(false)
    } else {
        eprintln!("outcome: Failed(no patch)");
        Ok(false)
    }
}

fn run_solve(a: SolveArgs) -> anyhow::Result<bool> {
    // --- Phase 2 of strict mode: grade an existing model patch (no agent) ---
    if let Some(patch_path) = &a.check_patch {
        if a.targets.is_empty() {
            bail!("--check-patch requires at least one --target");
        }
        let host = DetectorHost::with_bundled().context("load detectors")?;
        let repo = a
            .repo
            .canonicalize()
            .with_context(|| format!("repo path {:?}", a.repo))?;
        let model_patch =
            std::fs::read_to_string(patch_path).with_context(|| format!("read {patch_path:?}"))?;
        let inst = Instance {
            repo_root: repo,
            targets: a.targets,
            keep_green: a.keep_green,
            base_sha: a.base_sha,
        };
        let report = check_model_patch(&inst, &host, &model_patch)?;
        eprintln!("outcome: {:?}", report.outcome);
        eprintln!("{}", report.timings.report());
        return Ok(report.outcome.is_accepted());
    }

    // --- Shadow-verify: blind tests + full multi-attempt gate in a worktree ---
    if a.shadow_test_patch.is_some() {
        if a.common.agent_only {
            bail!("--shadow-test-patch conflicts with --agent-only");
        }
        return run_shadow_verify_solve(a);
    }

    let strategy = a.common.strategy()?;
    eprintln!("strategy: {}", strategy.name);
    let (provider, key) = a.common.resolve_provider()?;
    let issue = match (a.issue, a.issue_file) {
        (Some(s), _) => s,
        (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("read {f:?}"))?,
        (None, None) => bail!("provide --issue or --issue-file"),
    };

    // --- Phase 1 of strict mode: agent only, no harness gates ---
    if a.common.agent_only {
        eprintln!("mode: agent-only (no baseline/reproduce/verify)");
        let repo = a
            .repo
            .canonicalize()
            .with_context(|| format!("repo path {:?}", a.repo))?;
        if !is_git_repo(&repo) {
            bail!("agent-only requires a git repo at {repo:?}");
        }
        let ws = GitWorkspace::new(repo.clone());
        let mut executor = AgentExecutor::new(
            repo,
            issue,
            a.targets.clone(),
            a.keep_green.clone(),
            AgentConfig {
                model: a.common.model.clone(),
                api_key: key,
                max_turns_per_attempt: a.common.max_turns,
                provider: build_provider(&provider),
                strategy,
                naive: a.common.no_strategy,
                tool_policy: a.common.tool_policy()?,
                recorder: a.common.make_recorder()?,
                steering: None,
                // Strict issue-only: never spoon-feed targets even if passed.
                hide_targets: true,
                opaque_verdicts: false,
            },
        )
        .context("build agent executor")?;

        let mut changed = false;
        for attempt in 1..=a.common.max_attempts {
            if executor.attempt(attempt, None)? {
                changed = true;
                // Keep going for remaining attempts only if still failing self-check;
                // agent-only has no verify — one productive attempt is enough.
                break;
            }
        }
        let stats = executor.session_stats();
        eprintln!("session: {}", stats.summary());
        eprintln!("{}", executor.session_usage().report());
        let patch = if changed {
            ws.diff().unwrap_or_default()
        } else {
            String::new()
        };
        // Always roll back the tree so the host can apply test_patch cleanly.
        let _ = ws.reset();
        if patch.trim().is_empty() {
            eprintln!("outcome: Failed(no patch produced)");
            return Ok(false);
        }
        match &a.out {
            Some(path) => {
                std::fs::write(path, &patch).with_context(|| format!("write patch to {path:?}"))?;
                eprintln!("patch written to {path:?} ({} bytes)", patch.len());
            }
            None => println!("{patch}"),
        }
        eprintln!("outcome: AgentPatch (ungraded)");
        return Ok(true);
    }

    if a.targets.is_empty() {
        bail!("provide at least one --target (or use --agent-only / --check-patch)");
    }

    let host = DetectorHost::with_bundled().context("load detectors")?;
    let mut cache = BaselineCache::in_memory();
    let recorder = a.common.make_recorder()?;
    let ctx = SolveCtx {
        common: &a.common,
        provider: &provider,
        api_key: &key,
        host: &host,
        strategy,
        tool_policy: a.common.tool_policy()?,
        recorder,
    };

    let job = Job {
        id: "solve".to_string(),
        repo: a.repo,
        targets: a.targets,
        keep_green: a.keep_green,
        issue,
        base_sha: a.base_sha,
        use_workspace: !a.no_patch,
    };
    let report = solve_one(job, &ctx, &mut cache)?;

    eprintln!("outcome: {:?}", report.outcome);
    if let Some(patch) = &report.patch {
        match &a.out {
            Some(path) => {
                std::fs::write(path, patch).with_context(|| format!("write patch to {path:?}"))?;
                eprintln!("patch written to {path:?} ({} bytes)", patch.len());
            }
            None => println!("{patch}"),
        }
    }
    Ok(report.outcome.is_accepted())
}

fn run_batch(a: BatchArgs) -> anyhow::Result<()> {
    let strategy = a.common.strategy()?;
    eprintln!("strategy: {}", strategy.name);
    let (provider, key) = a.common.resolve_provider()?;
    let text = std::fs::read_to_string(&a.dataset)
        .with_context(|| format!("read dataset {:?}", a.dataset))?;
    if let Some(dir) = &a.out_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("create out-dir {dir:?}"))?;
    }

    let host = DetectorHost::with_bundled().context("load detectors")?;
    let mut cache = BaselineCache::in_memory();
    let ctx = SolveCtx {
        common: &a.common,
        provider: &provider,
        api_key: &key,
        host: &host,
        strategy,
        tool_policy: a.common.tool_policy()?,
        recorder: a.common.make_recorder()?,
    };
    let mut attribution = Attribution::new();
    let mut timings = pirs_bench::Timings::new();

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let inst: BatchInstance = serde_json::from_str(line)
            .with_context(|| format!("parse dataset line {}", lineno + 1))?;
        let id = inst.id.clone();

        let job = Job {
            id: id.clone(),
            repo: inst.repo,
            targets: inst.targets,
            keep_green: inst.keep_green,
            issue: inst.issue,
            base_sha: inst.base_sha,
            use_workspace: true,
        };
        let report = match solve_one(job, &ctx, &mut cache) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[{id}] error: {e:#}");
                continue;
            }
        };

        eprintln!("[{id}] {:?}", report.outcome);
        attribution.record(&report.outcome);
        timings.merge(&report.timings);
        if let (Some(dir), Some(patch)) = (&a.out_dir, &report.patch) {
            let path = dir.join(format!("{id}.patch"));
            std::fs::write(&path, patch).with_context(|| format!("write {path:?}"))?;
        }
    }

    println!("{}", attribution.report());
    println!("aggregate {}", timings.report());
    Ok(())
}

fn run_selftest(a: SelftestArgs) -> anyhow::Result<u8> {
    let mode = if a.agent {
        let strategy = a.common.strategy()?;
        eprintln!("strategy: {}", strategy.name);
        let (provider, api_key) = a.common.resolve_provider()?;
        selftest::Mode::Agent(Box::new(selftest::AgentMode {
            provider,
            model: a.common.model.clone(),
            api_key,
            max_turns: a.common.max_turns,
            strategy,
            tool_policy: a.common.tool_policy()?,
        }))
    } else {
        selftest::Mode::Oracle
    };

    let recorder = a.common.make_recorder()?;
    let report = selftest::run_selftest(&a.dir, a.count, &mode, recorder.as_ref())?;
    println!("{}", report.attribution.report());
    if !report.usage.is_empty() {
        println!("{}", report.usage.report());
    }
    println!("aggregate {}", report.timings.report());

    // Oracle mode must solve everything — any miss is a harness defect. Agent
    // mode is model-limited, so we report but don't hard-fail on misses.
    if !report.failures.is_empty() {
        eprintln!("{} instance(s) not solved:", report.failures.len());
        for f in &report.failures {
            eprintln!("  {f}");
        }
        if !a.agent {
            return Ok(1);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn common(no_strategy: bool) -> Common {
        Common {
            model: "m".into(),
            plan_model: None,
            provider: ProviderKind::Anthropic,
            base_url: None,
            max_attempts: 3,
            max_turns: 40,
            strategy: StrategyKind::Monolithic,
            strategy_script: None,
            no_strategy,
            profile: None,
            trace: None,
            hide_targets: false,
            agent_only: false,
        }
    }

    #[test]
    fn no_strategy_flag_resolves_to_empty_strategy() {
        let s = common(true).strategy().unwrap();
        assert_eq!(s.name, "none");
        assert!(s.steps.is_empty(), "naive mode has no phases: {s:?}");
        assert!(s.persist_across_attempts);
    }

    #[test]
    fn without_no_strategy_falls_back_to_selected_builtin() {
        let s = common(false).strategy().unwrap();
        assert_eq!(s.name, "monolithic");
        assert!(!s.steps.is_empty());
    }

    #[test]
    fn openai_compat_without_base_url_is_a_clear_error() {
        let mut c = common(false);
        c.provider = ProviderKind::OpenaiCompat;
        c.base_url = None;
        let err = c.resolve_provider().unwrap_err();
        assert!(err.to_string().contains("--base-url"), "{err}");
    }

    #[test]
    fn openai_compat_with_base_url_resolves_to_that_endpoint() {
        // SAFETY: single-threaded test process; no other test reads this var.
        std::env::set_var("CUSTOM_API_KEY", "test-key");
        let mut c = common(false);
        c.provider = ProviderKind::OpenaiCompat;
        c.base_url = Some("https://example.test/v1".to_string());
        let (provider, key) = c.resolve_provider().unwrap();
        assert_eq!(key, "test-key");
        match provider {
            Provider::OpenAiCompat { base_url, .. } => {
                assert_eq!(base_url, "https://example.test/v1")
            }
            other => panic!("expected OpenAiCompat, got {other:?}"),
        }
        std::env::remove_var("CUSTOM_API_KEY");
    }
}
