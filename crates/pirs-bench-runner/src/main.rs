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
    is_git_repo, run_instance, Attribution, BaselineCache, DetectorHost, GitWorkspace, Instance,
    InstanceReport,
};
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
}

impl ProviderKind {
    /// Resolve to a [`Provider`] plus the API key read from the matching env var.
    fn resolve(self) -> anyhow::Result<(Provider, String)> {
        let (provider, env) = match self {
            ProviderKind::Anthropic => (Provider::Anthropic, "ANTHROPIC_API_KEY"),
            ProviderKind::Deepseek => (Provider::deepseek(), "DEEPSEEK_API_KEY"),
        };
        let key = std::env::var(env).map_err(|_| anyhow::anyhow!("{env} is not set"))?;
        Ok((provider, key))
    }
}

/// Knobs shared by all modes.
#[derive(Args, Debug, Clone)]
struct Common {
    /// Model id to drive the agent with.
    #[arg(long, default_value = "claude-opus-4-8", global = true)]
    model: String,
    /// LLM backend.
    #[arg(long, value_enum, default_value_t = ProviderKind::Anthropic, global = true)]
    provider: ProviderKind,
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
}

impl Common {
    /// Load the profile if `--profile` was given.
    fn profile(&self) -> anyhow::Result<Option<Profile>> {
        match &self.profile {
            Some(path) => pirs_rhai::profile_script::load_profile_file(path).map(Some),
            None => Ok(None),
        }
    }

    /// The loop strategy to run. A `--profile` wins (its resolved strategy bakes in
    /// persona + model), then `--strategy-script`, then the selected built-in.
    fn strategy(&self) -> anyhow::Result<Strategy> {
        if self.no_strategy {
            // No phases at all: AgentConfig.naive (set from this same flag) makes
            // AgentExecutor bypass the strategy engine. This value only names the
            // run in traces/logs.
            return Ok(Strategy {
                name: "none".to_string(),
                steps: Vec::new(),
                persist_across_attempts: true,
            });
        }
        if let Some(profile) = self.profile()? {
            return Ok(profile.resolved_strategy());
        }
        match &self.strategy_script {
            Some(path) => pirs_rhai::strategy_script::load_strategy_file(path),
            None => Ok(self.strategy.into()),
        }
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
}

impl From<StrategyKind> for Strategy {
    fn from(k: StrategyKind) -> Self {
        // Built-ins now live as embedded scripts in pirs-rhai; resolve by name.
        let name = match k {
            StrategyKind::Monolithic => "monolithic",
            StrategyKind::PlanExec => "plan-exec",
            StrategyKind::PlanCriticExec => "plan-critic-exec",
            StrategyKind::WidePlanExec => "wide-plan-exec",
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
    #[arg(short = 't', long = "target", required = true)]
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
        },
    )
    .context("build agent executor")?;

    let inst = Instance {
        repo_root: repo,
        targets: job.targets,
        keep_green: job.keep_green,
        base_sha,
    };
    let report = run_instance(
        &inst,
        ctx.host,
        cache,
        &mut executor,
        ctx.common.max_attempts,
        workspace.as_ref(),
    )?;
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

fn run_solve(a: SolveArgs) -> anyhow::Result<bool> {
    let strategy = a.common.strategy()?;
    eprintln!("strategy: {}", strategy.name);
    let (provider, key) = a.common.provider.resolve()?;
    let issue = match (a.issue, a.issue_file) {
        (Some(s), _) => s,
        (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("read {f:?}"))?,
        (None, None) => bail!("provide --issue or --issue-file"),
    };
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
    let (provider, key) = a.common.provider.resolve()?;
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
        let (provider, api_key) = a.common.provider.resolve()?;
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
            provider: ProviderKind::Anthropic,
            max_attempts: 3,
            max_turns: 40,
            strategy: StrategyKind::Monolithic,
            strategy_script: None,
            no_strategy,
            profile: None,
            trace: None,
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
}
