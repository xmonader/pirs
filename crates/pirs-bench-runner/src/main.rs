//! `pirs-bench` — run the pirs coding agent against a benchmark instance under
//! the trustworthy verification harness, and emit the fix as a patch.
//!
//! The repo is expected to already be checked out at the instance's base commit.
//! The agent edits it in place; on an accepted outcome the unified diff is
//! printed (or written with `--out`), and on failure the tree is rolled back to
//! pristine so nothing partial leaks out.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context as _};
use clap::Parser;
use pirs_bench::{is_git_repo, run_instance, BaselineCache, DetectorHost, GitWorkspace, Instance};
use pirs_bench_runner::AgentExecutor;

#[derive(Parser, Debug)]
#[command(name = "pirs-bench", about = "Run the pirs agent against a benchmark task under verification")]
struct Cli {
    /// Path to the repository, already checked out at the base commit.
    repo: PathBuf,

    /// Failing test id to fix (repeatable). These are the FAIL_TO_PASS targets.
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

    /// Model id to drive the agent with.
    #[arg(long, default_value = "claude-opus-4-8")]
    model: String,

    /// Max verify-gated fix attempts before giving up.
    #[arg(long, default_value_t = 3)]
    max_attempts: u32,

    /// Max agent turns per attempt (the per-attempt budget).
    #[arg(long, default_value_t = 40)]
    max_turns: usize,

    /// Base commit SHA (for baseline caching). Defaults to the repo's HEAD.
    #[arg(long)]
    base_sha: Option<String>,

    /// Write the resulting patch here instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Don't extract a patch or roll back — just report the outcome.
    #[arg(long)]
    no_patch: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1), // ran cleanly, but did not solve
        Err(e) => {
            eprintln!("pirs-bench: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<bool> {
    let repo = cli.repo.canonicalize().with_context(|| format!("repo path {:?}", cli.repo))?;

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY is not set"))?;

    let issue = match (cli.issue, cli.issue_file) {
        (Some(s), _) => s,
        (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("read {f:?}"))?,
        (None, None) => bail!("provide --issue or --issue-file"),
    };

    // A git workspace (for patch extraction + rollback) when the repo is a git
    // checkout and patching wasn't disabled.
    let workspace = if cli.no_patch {
        None
    } else if is_git_repo(&repo) {
        Some(GitWorkspace::new(repo.clone()))
    } else {
        eprintln!("warning: {repo:?} is not a git repo; running without patch extraction/rollback");
        None
    };

    // Base SHA: explicit, else the workspace HEAD, else uncached.
    let base_sha = match (cli.base_sha, &workspace) {
        (Some(s), _) => Some(s),
        (None, Some(ws)) => ws.head_sha().ok(),
        (None, None) => None,
    };

    let host = DetectorHost::with_bundled().context("load detectors")?;
    let mut cache = BaselineCache::in_memory();
    let mut executor = AgentExecutor::new(
        repo.clone(),
        issue,
        cli.targets.clone(),
        cli.model,
        api_key,
        cli.max_turns,
    )
    .context("build agent executor")?;

    let inst = Instance {
        repo_root: repo,
        targets: cli.targets,
        keep_green: cli.keep_green,
        base_sha,
    };

    let report = run_instance(
        &inst,
        &host,
        &mut cache,
        &mut executor,
        cli.max_attempts,
        workspace.as_ref(),
    )?;

    eprintln!("outcome: {:?}", report.outcome);

    if let Some(patch) = &report.patch {
        match &cli.out {
            Some(path) => {
                std::fs::write(path, patch).with_context(|| format!("write patch to {path:?}"))?;
                eprintln!("patch written to {path:?} ({} bytes)", patch.len());
            }
            None => {
                println!("{patch}");
            }
        }
    }

    Ok(report.outcome.is_accepted())
}
