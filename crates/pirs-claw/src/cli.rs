//! Clap CLI surface for the `pirs-claw` binary.
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use pirs_claw::presets::{DEFAULT_MODEL, DEFAULT_PLAN_MODEL, DEFAULT_STRATEGY};

#[derive(Parser, Debug)]
#[command(
    name = "pirs-claw",
    about = "Agent: code + chat + schedule + gateway (telegram/discord/slack/whatsapp/signal). Exec: local|docker|ssh.",
    long_about = "Hermes-class personal agent over the pirs core.\n\
                  \n\
                  Coding:  pirs-claw -C ~/repo \"fix tests\"\n\
                  Chat:    pirs-claw chat \"…\"\n\
                  Schedule: pirs-claw schedule tick --run\n\
                  Gateway: pirs-claw serve --channel telegram\n\
                  Exec:    --exec local|docker|docker:<image>|docker@ctr|ssh:user@host\n\
                  \n\
                  Not supported (by design): Modal, Daytona, Singularity.\n\
                  Harness TUI: pirs --mode tui …"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,

    #[arg(long, short = 'C', global = true)]
    pub cwd: Option<PathBuf>,

    #[arg(long, global = true, default_value = DEFAULT_MODEL)]
    pub model: String,

    #[arg(long, global = true, default_value = DEFAULT_PLAN_MODEL)]
    pub plan_model: String,

    #[arg(long, global = true, default_value = DEFAULT_STRATEGY)]
    pub strategy: String,

    #[arg(long, global = true)]
    pub max_turns: Option<usize>,

    #[arg(long, global = true)]
    pub sequential: bool,

    #[arg(long, global = true)]
    pub weak: bool,

    /// Shell backend: local | docker | docker:<image> | docker@container | ssh:user@host
    #[arg(long, global = true, default_value = "local")]
    pub exec: String,

    /// Extra skills directory (default also loads ~/.pirs/skills).
    #[arg(long, global = true)]
    pub skills_dir: Option<PathBuf>,

    /// Allow coding tools on gateway messages (default: chat-only tools off for safety).
    #[arg(long, global = true)]
    pub gateway_code: bool,

    /// Enable skill crystallize after substantial code/chat turns (default on for CLI).
    #[arg(long, global = true, default_value_t = true)]
    pub learn: bool,

    /// Disable learning loop for this invocation.
    #[arg(long, global = true, default_value_t = false)]
    pub no_learn: bool,

    /// Load Rhai extensions from ~/.pirs/extensions and .pirs/extensions (chat/code).
    /// Default: on for CLI chat/code; use --no-extensions to disable.
    #[arg(long, global = true, default_value_t = false)]
    pub no_extensions: bool,

    /// Also load extensions on gateway messages (default off — fail-closed surface).
    #[arg(long, global = true, default_value_t = false)]
    pub gateway_extensions: bool,

    #[command(subcommand)]
    pub cmd: Option<Commands>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub prompt: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Code { prompt: Vec<String> },
    Chat { message: Vec<String> },
    History {
        #[arg(long, default_value_t = 20)]
        last: usize,
    },
    /// Search FTS memory.
    Recall {
        query: Vec<String>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Skills manager (list / show / add / usage).
    Skills {
        #[command(subcommand)]
        cmd: Option<SkillsCmd>,
    },
    /// List or search multi-key sessions under state dir.
    Sessions {
        #[command(subcommand)]
        cmd: Option<SessionsCmd>,
    },
    /// Transcribe audio file (multi-backend STT: registry → Groq/OpenAI → CLI).
    Transcribe {
        path: PathBuf,
    },
    /// Speech STT/TTS status and setup (cloud failover + local daemon helper).
    Speech {
        #[command(subcommand)]
        cmd: SpeechCmd,
    },
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },
    /// Multi-channel gateway (Hermes messaging gap).
    Serve {
        #[arg(long, default_value = "telegram")]
        channel: String,
    },
    /// Gateway / runtime status (pairing, schedule, speech, locks).
    Status,
    /// User soul/profile + skills curator (learning loop).
    Soul {
        #[command(subcommand)]
        cmd: SoulCmd,
    },
    /// Manage gateway pairing allowlist (add/list/remove peers).
    Pair {
        #[command(subcommand)]
        cmd: PairCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionsCmd {
    /// List session files (default).
    List,
    /// Full-text search across all session transcripts.
    Search {
        query: Vec<String>,
        #[arg(long, default_value_t = 12)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
pub enum PairCmd {
    /// List allowlisted peer ids.
    List,
    /// Add a peer id (telegram chat_id, etc.). Accepts `telegram:123` prefixes.
    Add { peer: String },
    /// Remove a peer id.
    Remove { peer: String },
    /// Mint a short code the unpaired peer can DM to self-pair (default TTL 10m).
    Code {
        /// Seconds until the code expires (60–86400).
        #[arg(long, default_value_t = 600)]
        ttl: u64,
    },
}

#[derive(Subcommand, Debug)]
pub enum SpeechCmd {
    /// Show resolved STT/TTS backend chain (no secrets).
    Status,
    /// Write speech backends into ~/.pirs/config.toml from available keys / local daemon.
    Setup {
        /// Enable cloud STT failover (Groq Whisper and/or OpenAI) from secrets.env keys.
        #[arg(long)]
        cloud: bool,
        /// Install/configure a local OpenAI-compatible speech daemon (Parakeet/Kokoro via helper script).
        #[arg(long)]
        local: bool,
        /// Local daemon base URL (default http://127.0.0.1:8090/v1).
        #[arg(long, default_value = "http://127.0.0.1:8090/v1")]
        local_url: String,
        /// Overwrite existing speech stanzas in config.toml.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SkillsCmd {
    /// List installed skills (default).
    List,
    /// Show one skill body.
    Show { name: String },
    /// Install a skill file or directory into ~/.pirs/skills.
    Add { path: PathBuf },
    /// Install SKILL.md from an HTTP(S) URL (agentskills.io layout).
    Install { url: String },
    /// Validate skill name/description (agentskills.io rules).
    Validate {
        /// Path to SKILL.md or skill directory, or installed skill name.
        target: String,
    },
    /// Remove an installed skill by name.
    Remove { name: String },
    /// Show skill usage counts.
    Usage,
}

#[derive(Subcommand, Debug)]
pub enum ScheduleCmd {
    Add {
        prompt: Vec<String>,
        /// Delay before first fire: seconds or 30s/5m/2h/1d
        #[arg(long = "in", default_value = "0")]
        in_dur: String,
        /// Repeat interval: seconds or 30s/5m/2h/1d (0 = one-shot)
        #[arg(long = "every", default_value = "0")]
        every_dur: String,
        /// Cron expression (5- or 6-field). When set, overrides --every.
        /// Examples: "0 9 * * 1-5" (weekdays 09:00), "*/15 * * * *" (every 15m)
        #[arg(long)]
        cron: Option<String>,
        /// Natural language schedule, e.g. "weekdays at 9:00", "every 15 minutes"
        #[arg(long = "nl")]
        nl: Option<String>,
        /// Named blueprint (morning-brief, standup, weekly-review, heartbeat, eod)
        #[arg(long)]
        blueprint: Option<String>,
        /// Blueprint slot: time=08:30 (repeatable)
        #[arg(long = "slot", value_name = "KEY=VALUE")]
        slots: Vec<String>,
        #[arg(long, default_value = "cli")]
        deliver: String,
        /// Optional job name (for pause/resume/remove by name).
        #[arg(long)]
        name: Option<String>,
        /// Attach skill(s) by name (repeatable); full body injected on fire.
        #[arg(long = "skill")]
        skills: Vec<String>,
        /// Optional model pin for this job.
        #[arg(long)]
        model: Option<String>,
    },
    List,
    /// List named automation blueprints.
    Blueprint {
        #[command(subcommand)]
        cmd: Option<BlueprintCmd>,
    },
    Pause { id: String },
    Resume { id: String },
    Remove { id: String },
    /// Fire one job immediately (does not wait for next_fire).
    Run { id: String },
    Tick {
        #[arg(long)]
        run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BlueprintCmd {
    /// List blueprints (default).
    List,
}

#[derive(Subcommand, Debug)]
pub enum SoulCmd {
    /// Print ~/.pirs/soul.md (user profile).
    Show,
    /// Write stdin or --text into soul.md
    Set {
        #[arg(long)]
        text: Option<String>,
    },
    /// Print skills curator report (usage + soul path).
    Curator,
}
