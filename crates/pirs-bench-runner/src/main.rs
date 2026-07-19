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

use anyhow::{bail, Context as _};
use clap::{Args, Parser, Subcommand, ValueEnum};
use pirs_bench::{
    is_git_repo, run_instance, Attribution, BaselineCache, DetectorHost, GitWorkspace, Instance,
    InstanceReport,
};
use pirs_bench_runner::{build_provider, selftest, AgentConfig, AgentExecutor, Provider};
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(name = "pirs-bench", about = "Run the pirs agent against benchmark tasks under verification")]
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
}

/// Run one instance through the full harness. Shared by `solve` and `batch`.
fn solve_one(job: Job, ctx: &SolveCtx, cache: &mut BaselineCache) -> anyhow::Result<InstanceReport> {
    let repo = job.repo.canonicalize().with_context(|| format!("repo path {:?}", job.repo))?;

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
        },
    )
    .context("build agent executor")?;

    let inst = Instance { repo_root: repo, targets: job.targets, keep_green: job.keep_green, base_sha };
    let report = run_instance(&inst, ctx.host, cache, &mut executor, ctx.common.max_attempts, workspace.as_ref())?;
    // Surface this session's behavior + token cost.
    eprintln!("session: {}", executor.session_stats().summary());
    eprintln!("{}", executor.session_usage().report());
    Ok(report)
}

fn run_solve(a: SolveArgs) -> anyhow::Result<bool> {
    let (provider, key) = a.common.provider.resolve()?;
    let issue = match (a.issue, a.issue_file) {
        (Some(s), _) => s,
        (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("read {f:?}"))?,
        (None, None) => bail!("provide --issue or --issue-file"),
    };
    let host = DetectorHost::with_bundled().context("load detectors")?;
    let mut cache = BaselineCache::in_memory();
    let ctx = SolveCtx { common: &a.common, provider: &provider, api_key: &key, host: &host };

    let job = Job {
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
    let (provider, key) = a.common.provider.resolve()?;
    let text = std::fs::read_to_string(&a.dataset)
        .with_context(|| format!("read dataset {:?}", a.dataset))?;
    if let Some(dir) = &a.out_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("create out-dir {dir:?}"))?;
    }

    let host = DetectorHost::with_bundled().context("load detectors")?;
    let mut cache = BaselineCache::in_memory();
    let ctx = SolveCtx { common: &a.common, provider: &provider, api_key: &key, host: &host };
    let mut attribution = Attribution::new();

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let inst: BatchInstance = serde_json::from_str(line)
            .with_context(|| format!("parse dataset line {}", lineno + 1))?;
        let id = inst.id.clone();

        let job = Job {
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
        if let (Some(dir), Some(patch)) = (&a.out_dir, &report.patch) {
            let path = dir.join(format!("{id}.patch"));
            std::fs::write(&path, patch).with_context(|| format!("write {path:?}"))?;
        }
    }

    println!("{}", attribution.report());
    Ok(())
}

fn run_selftest(a: SelftestArgs) -> anyhow::Result<u8> {
    let mode = if a.agent {
        let (provider, api_key) = a.common.provider.resolve()?;
        selftest::Mode::Agent {
            provider,
            model: a.common.model.clone(),
            api_key,
            max_turns: a.common.max_turns,
        }
    } else {
        selftest::Mode::Oracle
    };

    let report = selftest::run_selftest(&a.dir, a.count, &mode)?;
    println!("{}", report.attribution.report());
    if !report.usage.is_empty() {
        println!("{}", report.usage.report());
    }

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
