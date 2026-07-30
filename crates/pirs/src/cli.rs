//! Clap CLI definition for the `pirs` binary.
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "pirs",
    about = "Rust port of the pi coding agent, extensible via rhai"
)]
pub struct Cli {
    /// One-shot prompt; if omitted, starts the interactive REPL.
    /// Collects all trailing args so pseudo-subcommands work unquoted
    /// (`pirs blame src/main.rs:42`, `pirs pack install <url> --yes`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub prompt: Vec<String>,

    /// Run mode: `repl` (default), `tui` (ratatui console), `web` (browser UI
    /// on localhost — full tools/packs/MCP, same agent as TUI), `rpc`
    /// (JSONL over stdio), or `acp` (Agent Client Protocol for editors).
    #[arg(long, default_value = "repl")]
    pub mode: String,

    /// Model id to use
    #[arg(short, long, env = "PIRS_MODEL", default_value = "gpt-4o-mini")]
    pub model: String,

    /// LLM provider: openai (OpenAI-compatible) or anthropic
    #[arg(long, env = "PIRS_PROVIDER", default_value = "openai")]
    pub provider: String,

    /// OpenAI-compatible base URL
    #[arg(long, env = "OPENAI_BASE_URL")]
    pub base_url: Option<String>,

    /// API key (falls back to the provider's auth store or env var)
    #[arg(long)]
    pub api_key: Option<String>,

    /// Resume the most recent session for this directory
    #[arg(long)]
    pub resume: bool,

    /// Run a multi-phase loop strategy for a one-shot prompt. Primary built-ins:
    /// `monolithic`, `plan-exec`, `plan-critic-exec` (alias `plan-exec-critic`),
    /// `weak-drive` (alias `advisor`: strong plan+review, weak exec+fixup).
    /// Also accepts other built-in names, `.pirs/strategies/<name>.rhai`, or a
    /// path to a .rhai script. No effect in the interactive REPL.
    /// Pair with `--plan-model` for strong-plan / weak-exec multi-model runs.
    #[arg(long)]
    pub strategy: Option<String>,

    /// Model for planning (and critique) phases only. Executor phases use
    /// `--model`. Enables the strong-planner / weak-executor pitch without
    /// editing strategy scripts, e.g.:
    ///   pirs --model cheap-model --plan-model strong-model --strategy plan-exec "…"
    #[arg(long)]
    pub plan_model: Option<String>,

    /// Run under a profile (a role: persona + model + strategy + tool policy +
    /// extension packs). Accepts a name resolved from .pirs/profiles/<name>.rhai
    /// (project then ~/.pirs), a built-in (`default`, `weak`), or a path to a
    /// .rhai script. Implies its strategy when used with strategy mode;
    /// --strategy overrides which strategy the profile runs.
    /// Pack selection: without this flag, built-in `default` (`packs: "*"`)
    /// loads the full catalog. Pass a custom profile to curate packs.
    #[arg(long)]
    pub profile: Option<String>,

    /// Shell command that verifies a strategy attempt succeeded (exit 0 = pass,
    /// e.g. "cargo test" or "pytest -x"). On failure its output is fed into the
    /// next attempt as the prior verdict, so the strategy re-plans against the
    /// real error. Only used with --strategy/--profile.
    #[arg(long)]
    pub verify: Option<String>,

    /// Max strategy attempts when --verify is set (retry on gate failure).
    /// Defaults to 3 with --verify, 1 otherwise.
    #[arg(long)]
    pub max_attempts: Option<u32>,

    /// Skip injecting the PageRank repo-map sketch into the system prompt
    /// (on by default when the code graph is enabled).
    #[arg(long)]
    pub no_repo_map: bool,

    /// Disable rhai extension loading
    #[arg(long)]
    pub no_extensions: bool,

    /// Disable MCP server connections (.mcp.json)
    #[arg(long)]
    pub no_mcp: bool,

    /// Shortcut for full autonomy: all tools + no approval prompts.
    /// Equivalent to `--autonomy full` (and sets approval=yolo).
    /// Without this flag, bare `--yolo` was previously ignored as an unknown
    /// trailing token and tools stayed on the default `edit` ladder.
    #[arg(long)]
    pub yolo: bool,

    /// Approval prompts only: auto | ask | yolo.
    /// Prefer `--yolo` or `--autonomy full` so the tool ladder is raised too.
    #[arg(long, env = "PIRS_APPROVAL", default_value = "auto")]
    pub approval: String,

    /// Low-level safety profile (prefer `--autonomy`): default | plan |
    /// accept-edits | auto-approve.
    #[arg(
        long = "agent-profile",
        env = "PIRS_AGENT_PROFILE",
        default_value = "default"
    )]
    pub agent_profile: String,

    /// Working directory for the session (project root). Equivalent to `cd DIR && pirs …`.
    /// Applied before config/registry/tools resolve. Env: `PIRS_CWD`.
    #[arg(long, env = "PIRS_CWD", value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Extra work-context roots (multi-repo). Repeatable. Paths resolve relative
    /// to the process cwd before `--cwd` chdir. Use `//name/path` in tools to
    /// pin a root by directory basename.
    #[arg(long = "also", value_name = "DIR", action = clap::ArgAction::Append)]
    pub also: Vec<PathBuf>,

    /// Named multi-root context from `~/.pirs/contexts.toml` (`[[context]]`).
    /// Combined with `--cwd` / `--also` if both are set (named context first).
    #[arg(long = "context", value_name = "NAME", env = "PIRS_CONTEXT")]
    pub context: Option<String>,

    /// Run this session inside a git worktree for the named branch (create or reuse
    /// under `.pirs/worktrees/<name>`). Session cwd becomes that worktree.
    #[arg(long, env = "PIRS_WORKTREE")]
    pub worktree: Option<String>,

    /// Retry failed/rate-limited requests up to N times
    #[arg(long, default_value = "0")]
    pub max_retries: u32,

    /// Disable automatic context compaction
    #[arg(long)]
    pub no_compaction: bool,

    /// Model context window in tokens (drives compaction threshold)
    #[arg(long, default_value = "128000")]
    pub context_window: u64,

    /// Disable the code graph (code_map/ast_edit tools, blast-radius notes)
    #[arg(long)]
    pub no_graph: bool,

    /// Cache the code graph in .pirs/graph.db and refresh it incrementally
    /// (re-parse only changed files). Speeds up warm starts on large repos;
    /// off by default. The cache is disposable and never source of truth.
    #[arg(long)]
    pub persist_graph: bool,

    /// Enable the semantic_search tool: natural-language code search via an
    /// embedding service (implies --persist-graph for the vector store). Point
    /// it at any OpenAI-compatible /v1/embeddings endpoint with the flags below.
    #[arg(long)]
    pub semantic: bool,

    /// Embeddings endpoint base URL (OpenAI-compatible), e.g. Ollama's
    /// http://localhost:11434/v1 [env: PIRS_EMBED_BASE_URL]
    #[arg(long, env = "PIRS_EMBED_BASE_URL")]
    pub embed_base_url: Option<String>,

    /// Embedding model id [env: PIRS_EMBED_MODEL]
    #[arg(long, env = "PIRS_EMBED_MODEL")]
    pub embed_model: Option<String>,

    /// API key for the embeddings endpoint (optional for local servers)
    /// [env: PIRS_EMBED_API_KEY]
    #[arg(long, env = "PIRS_EMBED_API_KEY")]
    pub embed_api_key: Option<String>,

    /// Max source chars embedded per symbol. Lower it for small-context models
    /// (e.g. 512 for all-minilm) to avoid the truncating fallback; big-context
    /// models can leave the default [env: PIRS_EMBED_MAX_CHARS]
    #[arg(long, env = "PIRS_EMBED_MAX_CHARS")]
    pub embed_max_chars: Option<usize>,

    /// Opt into SYNCHRONOUS inline embedding instead of the default background
    /// indexer: code_search embeds up to N symbols per call (and no background
    /// task runs). Useful for a one-shot that must build the index in-process.
    /// By default, indexing runs in the background and searches never block.
    #[arg(long, env = "PIRS_EMBED_BATCH_CAP")]
    pub embed_batch_cap: Option<usize>,

    /// Start with only core tools loaded; model loads more via use_tool
    #[arg(long)]
    pub tool_diet: bool,

    /// Execute tool calls one at a time (helps weaker models)
    #[arg(long)]
    pub sequential: bool,

    /// Weak-model hardening preset (CLI only): --tool-diet, --sequential,
    /// --max-retries at least 3, defaults --strategy to plan-exec when
    /// neither --strategy nor --profile is set, and auto-sets --verify from
    /// the project test ecosystem when possible. Does not change extension
    /// packs — those come from profile `default` (`packs: "*"`). Multi-model:
    /// pair with `--plan-model <strong>` so planning stays strong while this
    /// run's `--model` is the weak executor; or use phase `model:` / `--cascade`.
    #[arg(long)]
    pub weak: bool,

    /// Draft each turn with a cheaper model; escalate to the main model only when the draft is rejected
    #[arg(long)]
    pub cascade: Option<String>,

    /// JSONL flight recorder for this run (agent events + strategy phases).
    /// Omit PATH to write `~/.pirs/traces/<session>-<ts>-<pid>.jsonl`.
    /// Same schema as `pirs-bench --trace` (jq-friendly, crash-safe).
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "AUTO")]
    pub trace: Option<String>,

    /// Max agent turns (exit code 53 when hit)
    #[arg(long)]
    pub max_turns: Option<usize>,

    /// Max wall-clock seconds (exit code 54 when hit)
    #[arg(long)]
    pub max_wall_time: Option<u64>,

    /// Max tool calls (exit code 55 when hit)
    #[arg(long)]
    pub max_tool_calls: Option<usize>,

    /// Run the local web app (browser UI on localhost). Equivalent to
    /// `--mode web`. Full agent stack (tools, default profile packs, MCP).
    #[arg(long)]
    pub serve: bool,

    /// Port for --serve
    #[arg(long, default_value = "8477")]
    pub port: u16,

    /// Bind address for --serve
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,

    /// Auth token for --serve writes (default: generated per run)
    #[arg(long, env = "PIRS_SERVE_TOKEN")]
    pub serve_token: Option<String>,

    /// Allow --serve to bind non-loopback addresses
    #[arg(long)]
    pub serve_external: bool,

    /// Print how model/provider/base-url/approval were each resolved (cli
    /// flag / env var / project config / user config / default) and exit.
    #[arg(long)]
    pub show_config: bool,

    /// Print runtime doctor report (API keys present, toolchain, LSP, MCP,
    /// git, browser/CDP, computer-use, gh, soul/audit) and exit.
    #[arg(long)]
    pub doctor: bool,

    /// **Primary tool autonomy** (streamlined): `plan` | `edit` | `full`.
    /// - plan  — read-only (no writes/shell)
    /// - edit  — workspace edits; shell blocked
    /// - full  — all tools + no approval prompts (true yolo)
    /// Prefer this over stacking --permission-mode / --agent-profile / --approval.
    /// Env: `PIRS_AUTONOMY`. Aliases: yolo→full, act→edit, read-only→plan.
    #[arg(long = "autonomy", env = "PIRS_AUTONOMY")]
    pub autonomy: Option<String>,

    /// Permission ladder (low-level; prefer `--autonomy`):
    /// read-only | workspace-write | danger-full-access.
    /// Env: PIRS_PERMISSION_MODE.
    #[arg(long = "permission-mode", env = "PIRS_PERMISSION_MODE")]
    pub permission_mode: Option<String>,

    /// Alias for `--autonomy plan|edit|full` (legacy: plan|act).
    #[arg(long = "mode-dial", env = "PIRS_MODE_DIAL")]
    pub mode_dial: Option<String>,

    /// Named tool-policy preset for hybrid experiments:
    /// `full` | `edit-test` | `read-only` | `no-tools`.
    /// Maps into the same autonomy ladder (+ tool-diet / sequential for experiments).
    #[arg(long = "tool-preset", env = "PIRS_TOOL_PRESET")]
    pub tool_preset: Option<String>,
}
