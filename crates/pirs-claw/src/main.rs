//! pirs-claw — Hermes-class agent (local/docker/ssh; multi-channel gateway).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use pirs_agent::phase_agent::AgentPhaseDriver;
use pirs_agent::strategy::{run_strategy_async, PhaseReq, Task, ToolScope};
use pirs_agent::Agent;
use pirs_claw::channel::{Channel, CliChannel, InboundMessage, OutboundReply, GATEWAY_CHANNELS};
use pirs_claw::memory_bridge;
use pirs_claw::pairing::PairingAllowlist;
use pirs_claw::presets::{
    apply_code_defaults, build_code_agent, coding_system_prompt, coding_tools, looks_like_repo,
    resolve_code_strategy, CodeOptions, DEFAULT_MODEL, DEFAULT_PLAN_MODEL, DEFAULT_STRATEGY,
};
use pirs_claw::registry;
use pirs_claw::learn;
use pirs_skills::{
    default_skills_dir, find_skill, install_skill, install_skill_url, load_skills, remove_skill,
    skill_tools, skills_full_section, skills_prompt_section, usage_counts, validate_skill, Skill,
};
use pirs_tools::life_tools;
use pirs_claw::parse_duration_secs;
use pirs_claw::{
    apply_exec_backend, claw_system_prompt, default_state_dir, describe_exec_backend,
    empty_assistant_diag, extract_assistant_reply, load_secrets_env, require_llm_key,
    should_mark_schedule_fired, DeliverTarget, GatewayReply, ScheduleStore, SessionId,
    SessionStore,
};

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
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[arg(long, short = 'C', global = true)]
    cwd: Option<PathBuf>,

    #[arg(long, global = true, default_value = DEFAULT_MODEL)]
    model: String,

    #[arg(long, global = true, default_value = DEFAULT_PLAN_MODEL)]
    plan_model: String,

    #[arg(long, global = true, default_value = DEFAULT_STRATEGY)]
    strategy: String,

    #[arg(long, global = true)]
    max_turns: Option<usize>,

    #[arg(long, global = true)]
    sequential: bool,

    #[arg(long, global = true)]
    weak: bool,

    /// Shell backend: local | docker | docker:<image> | docker@container | ssh:user@host
    #[arg(long, global = true, default_value = "local")]
    exec: String,

    /// Extra skills directory (default also loads ~/.pirs/skills).
    #[arg(long, global = true)]
    skills_dir: Option<PathBuf>,

    /// Allow coding tools on gateway messages (default: chat-only tools off for safety).
    #[arg(long, global = true)]
    gateway_code: bool,

    /// Enable skill crystallize after substantial code/chat turns (default on for CLI).
    #[arg(long, global = true, default_value_t = true)]
    learn: bool,

    /// Disable learning loop for this invocation.
    #[arg(long, global = true, default_value_t = false)]
    no_learn: bool,

    /// Load Rhai extensions from ~/.pirs/extensions and .pirs/extensions (chat/code).
    /// Default: on for CLI chat/code; use --no-extensions to disable.
    #[arg(long, global = true, default_value_t = false)]
    no_extensions: bool,

    /// Also load extensions on gateway messages (default off — fail-closed surface).
    #[arg(long, global = true, default_value_t = false)]
    gateway_extensions: bool,

    #[command(subcommand)]
    cmd: Option<Commands>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    prompt: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
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
enum SessionsCmd {
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
enum PairCmd {
    /// List allowlisted peer ids.
    List,
    /// Add a peer id (telegram chat_id, etc.).
    Add { peer: String },
    /// Remove a peer id.
    Remove { peer: String },
}

#[derive(Subcommand, Debug)]
enum SpeechCmd {
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
enum SkillsCmd {
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
enum ScheduleCmd {
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
enum BlueprintCmd {
    /// List blueprints (default).
    List,
}

#[derive(Subcommand, Debug)]
enum SoulCmd {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();
    load_secrets_env();
    apply_exec_backend(&cli.exec)?;

    let state = cli.state_dir.clone().unwrap_or_else(default_state_dir);
    std::fs::create_dir_all(&state)?;
    let schedule_path = state.join("schedule.json");
    let cwd = cli
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let sequential = cli.sequential || cli.weak;
    let max_turns = cli.max_turns.or(Some(if cli.weak { 60 } else { 40 }));
    let skills = load_all_skills(&cwd, cli.skills_dir.as_deref());

    match cli.cmd {
        Some(Commands::Pair { cmd }) => {
            let path = PairingAllowlist::default_path(&state);
            let mut al = PairingAllowlist::open(&path)?;
            match cmd {
                PairCmd::List => {
                    let peers = al.list();
                    if peers.is_empty() {
                        println!("(empty allowlist at {})", path.display());
                    } else {
                        for p in peers {
                            println!("{p}");
                        }
                    }
                }
                PairCmd::Add { peer } => {
                    let added = al.add(&path, &peer)?;
                    if added {
                        println!("paired {peer} → {}", path.display());
                    } else {
                        println!("already paired: {peer}");
                    }
                }
                PairCmd::Remove { peer } => {
                    let removed = al.remove(&path, &peer)?;
                    if removed {
                        println!("unpaired {peer}");
                    } else {
                        println!("not in allowlist: {peer}");
                    }
                }
            }
        }
        Some(Commands::Serve { channel }) => {
            let allow_path = PairingAllowlist::default_path(&state);
            let allowlist = PairingAllowlist::open(&allow_path)?;
            let model = cli.model.clone();
            let state_c = state.clone();
            let cwd_c = cwd.clone();
            let skills_c = skills.clone();
            let gateway_code = cli.gateway_code;
            let ch = channel.clone();
            pirs_claw::gateway::run_gateway(
                &ch,
                &state,
                &allowlist,
                move |inbound| {
                    let model = model.clone();
                    let state_c = state_c.clone();
                    let cwd_c = cwd_c.clone();
                    let skills_c = skills_c.clone();
                    Box::pin(async move {
                        handle_gateway_message(
                            &state_c,
                            &cwd_c,
                            &model,
                            &inbound,
                            &skills_c,
                            gateway_code,
                        )
                        .await
                    })
                },
            )
            .await?;
        }
        Some(Commands::Code { prompt }) => {
            let text = prompt.join(" ");
            if text.is_empty() {
                anyhow::bail!("usage: pirs-claw code <prompt…>");
            }
            run_code(
                &cwd,
                &cli.model,
                &cli.plan_model,
                &cli.strategy,
                &text,
                max_turns,
                sequential,
                &skills,
                cli.learn && !cli.no_learn && learn::learn_enabled_cli(),
                !cli.no_extensions,
            )
            .await?;
        }
        Some(Commands::Chat { message }) => {
            let text = message.join(" ");
            if text.is_empty() {
                anyhow::bail!("usage: pirs-claw chat <message>");
            }
            run_chat(
                &state,
                &cli.model,
                &cwd,
                &text,
                &skills,
                cli.learn && !cli.no_learn && learn::learn_enabled_cli(),
                !cli.no_extensions,
            )
            .await?;
        }
        Some(Commands::History { last }) => {
            let store = SessionStore::open_for(&state, SessionId::cli_local())?;
            let lines = store.load()?;
            let start = lines.len().saturating_sub(last);
            for l in &lines[start..] {
                println!("[{}] {}: {}", l.ts, l.role, l.text);
            }
        }
        Some(Commands::Recall { query, limit }) => {
            let q = query.join(" ");
            let mem = memory_bridge::open_memory(&state)?;
            let ctx = memory_bridge::recall_context(&mem, &q, limit);
            if ctx.is_empty() {
                println!("(no memory hits for {q:?})");
            } else {
                print!("{ctx}");
            }
        }
        Some(Commands::Skills { cmd }) => {
            let cmd = cmd.unwrap_or(SkillsCmd::List);
            match cmd {
                SkillsCmd::List => {
                    if skills.is_empty() {
                        println!(
                            "(no skills under {} — use: pirs-claw skills add <path>)",
                            default_skills_dir().display()
                        );
                    }
                    for s in &skills {
                        println!("{} — {} ({})", s.name, s.description, s.path.display());
                    }
                }
                SkillsCmd::Show { name } => match find_skill(&skills, &name) {
                    Some(s) => {
                        println!("# {}\n{}\n\n{}", s.name, s.description, s.body);
                    }
                    None => anyhow::bail!("unknown skill {name:?}"),
                },
                SkillsCmd::Add { path } => {
                    let dest = install_skill(&path, &default_skills_dir())?;
                    println!("installed → {}", dest.display());
                }
                SkillsCmd::Install { url } => {
                    let dest = install_skill_url(&url, &default_skills_dir())?;
                    println!("installed from URL → {}", dest.display());
                }
                SkillsCmd::Validate { target } => {
                    let path = PathBuf::from(&target);
                    let sk = if path.exists() {
                        let skill_md = if path.is_dir() {
                            path.join("SKILL.md")
                        } else {
                            path
                        };
                        let raw = std::fs::read_to_string(&skill_md)?;
                        pirs_claw::skills::parse_skill_md(&raw, &skill_md)
                    } else if let Some(s) = find_skill(&skills, &target) {
                        s.clone()
                    } else {
                        anyhow::bail!("skill not found: {target}");
                    };
                    match validate_skill(&sk) {
                        Ok(()) => println!("ok: {} — {}", sk.name, sk.description),
                        Err(e) => anyhow::bail!("invalid: {e}"),
                    }
                }
                SkillsCmd::Remove { name } => {
                    if remove_skill(&name, &default_skills_dir())? {
                        println!("removed {name}");
                    } else {
                        println!("not found: {name}");
                    }
                }
                SkillsCmd::Usage => {
                    let u = usage_counts();
                    if u.is_empty() {
                        println!("(no usage recorded yet)");
                    }
                    for (k, v) in u {
                        println!("{k}\t{v}");
                    }
                }
            }
        }
        Some(Commands::Sessions { cmd }) => match cmd.unwrap_or(SessionsCmd::List) {
            SessionsCmd::List => {
                let root = state.join("sessions");
                if !root.is_dir() {
                    println!("(no sessions under {})", root.display());
                } else {
                    for ent in walkdir_sessions(&root) {
                        println!("{ent}");
                    }
                }
            }
            SessionsCmd::Search { query, limit } => {
                let q = query.join(" ");
                if q.trim().is_empty() {
                    anyhow::bail!("usage: pirs-claw sessions search <query>");
                }
                let hits = pirs_claw::session_search::search_sessions(&state, &q, limit)?;
                if hits.is_empty() {
                    println!("(no matches for {q:?})");
                } else {
                    for h in hits {
                        println!(
                            "[{}] score={} session={} role={}\n  {}\n",
                            h.path, h.score, h.session_key, h.role, h.snippet
                        );
                    }
                }
            }
        },
        Some(Commands::Status) => {
            print_runtime_status(&state, &schedule_path).await?;
        }
        Some(Commands::Soul { cmd }) => match cmd {
            SoulCmd::Show => {
                print!("{}", pirs_skills::read_soul());
            }
            SoulCmd::Set { text } => {
                let body = if let Some(t) = text {
                    t
                } else {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                };
                let p = pirs_skills::write_soul(&body)?;
                println!("wrote soul → {}", p.display());
            }
            SoulCmd::Curator => {
                print!(
                    "{}",
                    pirs_skills::curator_report(&default_skills_dir())
                );
            }
        },
        Some(Commands::Transcribe { path }) => {
            match pirs_claw::voice::transcribe_audio(&path).await? {
                Some(t) => println!("{t}"),
                None => anyhow::bail!(
                    "no transcription (configure STT: `pirs-claw speech setup --cloud`, \
                     PIRS_SPEECH_BASE_URL, whisper CLI, or PIRS_CLAW_TRANSCRIBE_CMD)"
                ),
            }
        }
        Some(Commands::Speech { cmd }) => match cmd {
            SpeechCmd::Status => {
                for line in pirs_ai::speech_status_lines_probed().await {
                    println!("{line}");
                }
            }
            SpeechCmd::Setup {
                cloud,
                local,
                local_url,
                force,
            } => {
                if !cloud && !local {
                    anyhow::bail!("pass --cloud and/or --local (see pirs-claw speech setup --help)");
                }
                pirs_claw::speech_setup::run_setup(pirs_claw::speech_setup::SetupOpts {
                    cloud,
                    local,
                    local_url,
                    force,
                })?;
            }
        },
        Some(Commands::Schedule { cmd }) => {
            let store = ScheduleStore::open(&schedule_path)?;
            match cmd {
                ScheduleCmd::Add {
                    prompt,
                    in_dur,
                    every_dur,
                    cron,
                    nl,
                    blueprint,
                    slots,
                    deliver,
                    name,
                    skills: job_skills,
                    model,
                } => {
                    let mut p = prompt.join(" ");
                    let in_secs = parse_duration_secs(&in_dur)?;
                    let mut every = parse_duration_secs(&every_dur)?;
                    let mut cron = cron;
                    if let Some(bp) = blueprint {
                        let mut map = std::collections::HashMap::new();
                        for s in &slots {
                            if let Some((k, v)) = s.split_once('=') {
                                map.insert(k.trim().to_string(), v.trim().to_string());
                            }
                        }
                        let (c, prompt_bp) = pirs_claw::cron_blueprints::expand_blueprint(
                            &bp,
                            &map,
                            if p.trim().is_empty() { None } else { Some(p.as_str()) },
                        )?;
                        cron = Some(c);
                        if p.trim().is_empty() {
                            p = prompt_bp;
                        }
                    } else if let Some(nl_s) = nl {
                        match pirs_claw::cron_blueprints::parse_nl_schedule(&nl_s)? {
                            pirs_claw::cron_blueprints::NlSchedule::Cron(c) => cron = Some(c),
                            pirs_claw::cron_blueprints::NlSchedule::EverySecs(secs) => {
                                every = secs;
                            }
                        }
                    }
                    if p.trim().is_empty() {
                        anyhow::bail!("schedule add needs a prompt (or --blueprint with defaults)");
                    }
                    let deliver = DeliverTarget::parse(&deliver);
                    let e = store.add_full_cron(
                        &p,
                        every,
                        in_secs,
                        cron,
                        deliver,
                        name,
                        job_skills,
                        model,
                    )?;
                    println!(
                        "scheduled {} name={:?} next_fire={} every_secs={} cron={:?} deliver={} skills={:?}",
                        e.id,
                        e.name,
                        e.next_fire,
                        e.every_secs,
                        e.cron,
                        e.deliver.as_config_str(),
                        e.skills
                    );
                }
                ScheduleCmd::Blueprint { cmd } => {
                    let _ = cmd;
                    print!("{}", pirs_claw::cron_blueprints::list_blueprints());
                }
                ScheduleCmd::List => {
                    for j in store.list()? {
                        println!(
                            "{} name={:?} enabled={} next={} every={} cron={:?} last_run={:?} last_status={:?} fail_count={} last_error={:?} deliver={} skills={:?} | {}",
                            j.id,
                            j.name,
                            j.enabled,
                            j.next_fire,
                            j.every_secs,
                            j.cron,
                            j.last_run,
                            j.last_status,
                            j.fail_count,
                            j.last_error,
                            j.deliver.as_config_str(),
                            j.skills,
                            j.prompt
                        );
                    }
                }
                ScheduleCmd::Pause { id } => {
                    if store.set_enabled(&id, false)? {
                        println!("paused {id}");
                    } else {
                        anyhow::bail!("job not found: {id}");
                    }
                }
                ScheduleCmd::Resume { id } => {
                    if store.set_enabled(&id, true)? {
                        println!("resumed {id}");
                    } else {
                        anyhow::bail!("job not found: {id}");
                    }
                }
                ScheduleCmd::Remove { id } => {
                    if store.remove(&id)? {
                        println!("removed {id}");
                    } else {
                        anyhow::bail!("job not found: {id}");
                    }
                }
                ScheduleCmd::Run { id } => {
                    let job = store
                        .find(&id)?
                        .ok_or_else(|| anyhow::anyhow!("job not found: {id}"))?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    match fire_schedule_job(&job, &state, &cli.model, &skills).await {
                        Ok(true) => {
                            store.mark_fired(&job.id, now)?;
                            println!("ran {} ok", job.id);
                        }
                        Ok(false) => {
                            store.mark_failed(&job.id, now, "fire returned false")?;
                            anyhow::bail!("job {} failed", job.id);
                        }
                        Err(e) => {
                            store.mark_failed(&job.id, now, &e.to_string())?;
                            return Err(e);
                        }
                    }
                }
                ScheduleCmd::Tick { run } => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    // Share cron lock with serve ticker so RMW cannot clobber concurrent fires.
                    let _cron_lock = if run {
                        match pirs_claw::instance_lock::try_acquire(&state, "cron") {
                            Ok(l) => Some(l),
                            Err(e) => {
                                eprintln!("[tick] cron lock busy ({e}); try again shortly");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    if run && _cron_lock.is_none() {
                        anyhow::bail!("could not acquire cron lock (another tick/serve running?)");
                    }
                    let due = store.due(now)?;
                    if due.is_empty() {
                        println!("no due jobs");
                    }
                    let mut ok_n = 0u32;
                    let mut fail_n = 0u32;
                    for j in due {
                        println!(
                            "due {} deliver={}: {}",
                            j.id,
                            j.deliver.as_config_str(),
                            j.prompt
                        );
                        if !run {
                            continue;
                        }
                        match fire_schedule_job(&j, &state, &cli.model, &skills).await {
                            Ok(true) if should_mark_schedule_fired(true, true) => {
                                store.mark_fired(&j.id, now)?;
                                ok_n += 1;
                            }
                            Ok(true) => {}
                            Ok(false) => {
                                store.mark_failed(&j.id, now, "fire returned false")?;
                                fail_n += 1;
                            }
                            Err(e) => {
                                store.mark_failed(&j.id, now, &e.to_string())?;
                                eprintln!("[tick] job {} error: {e}", j.id);
                                fail_n += 1;
                            }
                        }
                    }
                    if run {
                        println!(
                            "[tick summary] ok={ok_n} failed={fail_n} (failed jobs stay due for retry)"
                        );
                    }
                }
            }
        }
        None => {
            let text = cli.prompt.join(" ");
            if text.is_empty() {
                print_usage();
                std::process::exit(2);
            }
            let do_learn = cli.learn && !cli.no_learn && learn::learn_enabled_cli();
            let ext = !cli.no_extensions;
            if cli.cwd.is_some() || looks_like_repo(&cwd) {
                run_code(
                    &cwd,
                    &cli.model,
                    &cli.plan_model,
                    &cli.strategy,
                    &text,
                    max_turns,
                    sequential,
                    &skills,
                    do_learn,
                    ext,
                )
                .await?;
            } else {
                run_chat(&state, &cli.model, &cwd, &text, &skills, do_learn, ext).await?;
            }
        }
    }
    Ok(())
}

fn load_all_skills(cwd: &Path, extra: Option<&Path>) -> Vec<Skill> {
    let mut skills = pirs_skills::discover_skills(cwd);
    if let Some(d) = extra {
        for sk in load_skills(d) {
            if !skills.iter().any(|s| s.name == sk.name) {
                skills.push(sk);
            }
        }
    }
    // Always include default home skills dir even if discover missed (empty home).
    for sk in load_skills(&default_skills_dir()) {
        if !skills.iter().any(|s| s.name == sk.name) {
            skills.push(sk);
        }
    }
    skills
}

/// Chat-safe tool set: recall + progressive skills + life tools (+ optional code tools).
fn chat_safe_tools(
    cwd: &Path,
    skills: &[Skill],
    allow_code: bool,
    allow_skill_manage: bool,
) -> Vec<Arc<dyn pirs_agent::AgentTool>> {
    chat_safe_tools_with_state(cwd, skills, allow_code, allow_skill_manage, None, None)
}

/// Gateway/chat tools. When `state_dir` is set, `peer_scope` must be the caller's
/// `SessionId::key()` so `session_search` cannot read other peers' transcripts.
fn chat_safe_tools_with_state(
    cwd: &Path,
    skills: &[Skill],
    allow_code: bool,
    allow_skill_manage: bool,
    state_dir: Option<&Path>,
    peer_scope: Option<&str>,
) -> Vec<Arc<dyn pirs_agent::AgentTool>> {
    let skills_arc = Arc::new(skills.to_vec());
    let mut tools: Vec<Arc<dyn pirs_agent::AgentTool>> =
        vec![Arc::new(pirs_tools::RecallTool::default())];
    tools.extend(skill_tools(skills_arc, allow_skill_manage));
    tools.extend(life_tools(false));
    // Browser + vision on chat/gateway (SSRF-safe / path-contained).
    tools.extend(pirs_tools::browser_tools(cwd.to_path_buf()));
    #[cfg(feature = "cdp")]
    tools.extend(pirs_tools::cdp_tools(cwd.to_path_buf()));
    tools.extend(pirs_tools::vision_tools(cwd.to_path_buf()));
    // Desktop computer-use only when explicitly enabled (dangerous).
    if matches!(
        std::env::var("PIRS_COMPUTER_USE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    ) {
        tools.extend(pirs_tools::computer_tools(cwd.to_path_buf()));
    }
    if let Some(state) = state_dir {
        // Gateway: require explicit peer key on the tool instance (not env).
        if let Some(peer) = peer_scope {
            tools.push(pirs_claw::session_search::gateway_session_search_tool(
                state.to_path_buf(),
                peer,
            ));
        } else {
            // Owner/CLI path with state_dir but no peer: global search is OK.
            tools.push(pirs_claw::session_search::session_search_tool(
                state.to_path_buf(),
            ));
        }
    }
    if allow_code {
        tools.extend(coding_tools(cwd));
    }
    // Dedupe (coding_tools already includes browser/vision via default_tools).
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    tools
}

async fn print_runtime_status(state: &Path, schedule_path: &Path) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("state_dir: {}", state.display());
    let pair = PairingAllowlist::default_path(state);
    let al = PairingAllowlist::open(&pair)?;
    println!("pairing: {} ({} peer(s))", pair.display(), al.list().len());
    for p in al.list() {
        println!("  - {p}");
    }
    let tg_token = std::env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| std::env::var("PIRS_TELEGRAM_BOT_TOKEN"))
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false);
    println!("telegram_token: {}", if tg_token { "set" } else { "missing" });
    println!(
        "telegram_lock: {}",
        pirs_claw::instance_lock::lock_status(state, "telegram")
    );
    println!(
        "cron_lock: {}",
        pirs_claw::instance_lock::lock_status(state, "cron")
    );
    let store = ScheduleStore::open(schedule_path)?;
    let jobs = store.list()?;
    println!("schedule: {} job(s) at {}", jobs.len(), schedule_path.display());
    if let Some(next) = store.next_due()? {
        let in_secs = next.next_fire.saturating_sub(now);
        println!(
            "  next_due: {} in {}s (next_fire={})",
            next.name.as_deref().unwrap_or(&next.id),
            in_secs,
            next.next_fire
        );
    }
    for j in jobs.iter().take(8) {
        println!(
            "  {} enabled={} cron={:?} every={} next={} last_run={:?} status={:?} fails={} err={:?}",
            j.name.as_deref().unwrap_or(&j.id),
            j.enabled,
            j.cron,
            j.every_secs,
            j.next_fire,
            j.last_run,
            j.last_status,
            j.fail_count,
            j.last_error.as_ref().map(|e| {
                if e.chars().count() > 80 {
                    format!("{}…", e.chars().take(80).collect::<String>())
                } else {
                    e.clone()
                }
            })
        );
    }
    if jobs.len() > 8 {
        println!("  … +{} more", jobs.len() - 8);
    }
    let sessions = state.join("sessions");
    let n_sess = if sessions.is_dir() {
        walkdir_sessions(&sessions).len()
    } else {
        0
    };
    println!("sessions: {n_sess} file(s) under {}", sessions.display());
    let cdp = std::env::var("PIRS_BROWSER_CDP_URL")
        .or_else(|_| std::env::var("BROWSER_CDP_URL"))
        .or_else(|_| std::env::var("CDP_URL"))
        .ok();
    println!(
        "browser_cdp: {}",
        cdp.as_deref().unwrap_or("(auto-launch or default :9222)")
    );
    println!("speech (probed):");
    for line in pirs_ai::speech_status_lines_probed().await {
        println!("  {line}");
    }
    println!(
        "tts_on_voice_default: {} (backends={})",
        pirs_claw::voice::tts_on_voice(),
        pirs_claw::voice::tts_backends_configured()
    );
    Ok(())
}

/// Load optional Rhai packs for claw chat/code (not gateway unless flagged).
fn load_claw_extensions(cwd: &Path, enabled: bool) -> Option<Arc<pirs_rhai::ExtensionHost>> {
    if !enabled {
        return None;
    }
    pirs_rhai::register_core_host_apis();
    let mut host = pirs_rhai::ExtensionHost::new();
    if let Ok(p) = pirs_rhai::discover::resolve_pack_profile(None, cwd) {
        pirs_rhai::weak_packs::load_profile_packs(&mut host, &p.packs);
    } else {
        pirs_rhai::weak_packs::load_into(&mut host);
    }
    host.load_default_dirs(cwd);
    if !host.load_errors.is_empty() {
        for e in &host.load_errors {
            eprintln!("[pirs-claw extensions: {e}]");
        }
    }
    let host = Arc::new(host);
    let n = host.tools().len();
    if n > 0 || !host.load_errors.is_empty() {
        eprintln!(
            "[pirs-claw extensions: {} tool(s) from packs; host APIs project_profile/skills_index]",
            n
        );
    }
    Some(host)
}

/// Profile denials + optional extension packs + audit log (Opus review §2.4).
///
/// Gateway/chat peers previously had only the tool *list* as policy. This wires
/// the same profile gate + audit listener the `pirs` CLI uses. Interactive
/// approval prompts are not available on remote channels; use
/// `PIRS_AGENT_PROFILE=plan|accept-edits|auto-approve` (default: accept-edits
/// for interactive, plan for unattended).
fn install_claw_safety(
    mut agent: Agent,
    unattended: bool,
    host: Option<&Arc<pirs_rhai::ExtensionHost>>,
) -> Agent {
    let profile = if unattended {
        pirs_tools::SafetyProfile::parse(
            &std::env::var("PIRS_CLAW_UNATTENDED_PROFILE").unwrap_or_else(|_| "plan".into()),
        )
        .unwrap_or(pirs_tools::SafetyProfile::Plan)
    } else {
        pirs_tools::SafetyProfile::parse(
            &std::env::var("PIRS_AGENT_PROFILE")
                .or_else(|_| std::env::var("PIRS_CLAW_PROFILE"))
                .unwrap_or_else(|_| "accept-edits".into()),
        )
        .unwrap_or(pirs_tools::SafetyProfile::AcceptEdits)
    };
    std::env::set_var("PIRS_AGENT_PROFILE", profile.name());

    if let Some(host) = host {
        let mut tools = agent.tools.clone();
        tools.extend(host.tools());
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
        agent = agent.with_tools(tools);
    }

    // Profile denials first, then pack before_tool_call (first blocker wins).
    let profile_hook = pirs_tools::profile_hook(profile);
    let mut hooks = host.map(|h| h.hooks()).unwrap_or_default();
    let prev = hooks.before_tool_call.take();
    hooks.before_tool_call = Some(std::sync::Arc::new(move |id, name, args| {
        if let Some(r) = profile_hook(id, name, args) {
            return Some(r);
        }
        if let Some(ref p) = prev {
            return p(id, name, args);
        }
        None
    }));
    agent = agent.with_hooks(hooks);

    let audit = pirs_agent::AuditLog::default_open();
    if pirs_agent::audit_enabled() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            eprintln!("[pirs-claw audit: {}]", audit.path().display());
        });
    }
    agent.subscribe(pirs_agent::audit_listener(audit));
    agent
}

async fn fire_schedule_job(
    job: &pirs_claw::ScheduleEntry,
    state: &Path,
    default_model: &str,
    all_skills: &[Skill],
) -> anyhow::Result<bool> {
    let model = job.model.as_deref().unwrap_or(default_model);
    let attached = pirs_claw::skills::select_skills(all_skills, &job.skills);
    let prompt = if attached.is_empty() {
        job.prompt.clone()
    } else {
        format!(
            "{}\n\n{}",
            skills_full_section(&attached),
            job.prompt
        )
    };
    // Isolated job chat: use a temp state subdir so schedule doesn't pollute cli/local.
    let job_state = state.join("schedule-runs").join(&job.id);
    std::fs::create_dir_all(&job_state)?;
    let out = std::process::Command::new(std::env::current_exe()?)
        .arg("--model")
        .arg(model)
        .arg("--state-dir")
        .arg(&job_state)
        .arg("--no-learn")
        .env(pirs_claw::UNATTENDED_ENV, "1")
        .arg("chat")
        .arg(&prompt)
        .output()?;
    if !out.status.success() {
        eprintln!(
            "[tick] job {} chat failed: {}",
            job.id,
            String::from_utf8_lossy(&out.stderr)
        );
        return Ok(false);
    }
    let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let text = if reply.is_empty() {
        "(scheduled job finished with empty reply)".to_string()
    } else {
        reply
    };
    if let Err(e) = pirs_claw::gateway::deliver_outbound(&job.deliver, &text).await {
        eprintln!(
            "[tick] job {} deliver {} failed: {e}",
            job.id,
            job.deliver.as_config_str()
        );
        return Ok(false);
    }
    Ok(true)
}

fn walkdir_sessions(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(channels) = std::fs::read_dir(root) else {
        return out;
    };
    for ch in channels.flatten() {
        if !ch.path().is_dir() {
            continue;
        }
        let channel = ch.file_name().to_string_lossy().into_owned();
        let Ok(peers) = std::fs::read_dir(ch.path()) else {
            continue;
        };
        for pe in peers.flatten() {
            let name = pe.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".jsonl") {
                continue;
            }
            let peer = name.trim_end_matches(".jsonl");
            let meta_path = pe.path().with_file_name(format!("{peer}.meta.json"));
            let extra = std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .map(|v| {
                    format!(
                        " msgs={} last={}",
                        v.get("message_count").and_then(|x| x.as_u64()).unwrap_or(0),
                        v.get("last_active").and_then(|x| x.as_u64()).unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            out.push(format!("{channel}/{peer}{extra}"));
        }
    }
    out.sort();
    out
}

fn print_usage() {
    eprintln!(
        "pirs-claw — code + chat + schedule + gateway\n\
         \n\
         pirs-claw -C <repo> \"fix …\"\n\
         pirs-claw chat \"…\"\n\
         pirs-claw recall \"keyword\"\n\
         pirs-claw sessions\n\
         pirs-claw skills list|show|add|usage\n\
         pirs-claw schedule add --in 5m --every 1h \"…\"\n\
         pirs-claw schedule tick [--run]\n\
         pirs-claw serve --channel telegram|discord|slack|whatsapp|signal\n\
         pirs-claw pair list|add|remove\n\
         pirs-claw --exec docker|ssh:user@host …\n\
         \n\
         defaults: model={DEFAULT_MODEL} plan_model={DEFAULT_PLAN_MODEL} strategy={DEFAULT_STRATEGY}\n\
         exec backends: local, docker, docker:<image>, docker@ctr, ssh:user@host\n\
         (not supported: modal, daytona, singularity)\n\
         registry: ~/.pirs/config.toml + secrets.env (same as pirs)\n\
         gateway channels: {}",
        GATEWAY_CHANNELS.join(", ")
    );
}

async fn handle_gateway_message(
    state: &Path,
    cwd: &Path,
    model: &str,
    inbound: &InboundMessage,
    skills: &[Skill],
    allow_code_tools: bool,
) -> anyhow::Result<GatewayReply> {
    // Gateway: skill writes off unless PIRS_SKILL_WRITE=1 explicitly.
    if std::env::var("PIRS_SKILL_WRITE").is_err() && std::env::var("PIRS_CLAW_SKILL_WRITE").is_err()
    {
        std::env::set_var("PIRS_SKILL_WRITE", "0");
    }
    let sid = SessionId::from_inbound(inbound);
    let store = SessionStore::open_for(state, sid.clone())?;
    store.append("user", &inbound.text)?;
    let mem = memory_bridge::open_memory(state).ok();
    if let Some(ref m) = mem {
        memory_bridge::scope_session(m, &sid.key());
        memory_bridge::remember_turn(m, "user", &inbound.text);
    }
    let (provider, key, _) = registry::resolve_llm(model, 2)?;
    require_llm_key(key.as_deref())?;
    let key_for_learn = key.clone();
    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let mut sys = claw_system_prompt();
    sys.push_str(&skills_prompt_section(skills));
    if allow_code_tools {
        sys.push_str(&pirs_tools::detect_profile(cwd).prompt_section());
    }
    if let Some(ref m) = mem {
        sys.push_str(&memory_bridge::recall_context(m, &inbound.text, 5));
    }
    let attach_log = pirs_claw::attach::AttachmentLog::new();
    let out_dir = state.join("outbound").join(sid.key().replace('/', "_"));
    // Scope session_search to this peer only (never global on multi-tenant gateway).
    let mut tools = chat_safe_tools_with_state(
        cwd,
        skills,
        allow_code_tools,
        false,
        Some(state),
        Some(sid.key().as_str()),
    );
    tools.push(Arc::new(pirs_claw::attach::AttachFileTool::new(
        out_dir.clone(),
        attach_log.clone_handle(),
    )));
    let mut agent = Agent::new(provider.clone(), model)
        .with_system_prompt(sys)
        .with_tools(tools)
        .with_completion(completion);
    agent = install_claw_safety(agent, pirs_claw::is_unattended(), None);
    if let Ok(mut msgs) = store.to_agent_messages() {
        if let Some(pirs_ai::Message::User(_)) = msgs.last() {
            msgs.pop();
        }
        agent.messages = msgs;
    }
    let new_msgs = agent.prompt(&inbound.text).await?;
    let reply = extract_assistant_reply(&new_msgs).ok_or_else(|| {
        anyhow::anyhow!(
            "empty assistant reply ({})",
            empty_assistant_diag(&new_msgs)
        )
    })?;
    store.append("assistant", &reply)?;
    if let Some(ref m) = mem {
        memory_bridge::remember_turn(m, "assistant", &reply);
    }
    if learn::learn_enabled_gateway() {
        learn::maybe_memory_nudge(
            provider.clone(),
            model,
            key_for_learn.clone(),
            state,
            &sid.key(),
            &inbound.text,
            &reply,
        )
        .await;
        // Improve skills that were viewed this turn (Hermes-style self-improve).
        let transcript = learn::session_transcript(&inbound.text, &reply, "gateway");
        // Long Telegram threads can crystallize skills (same gate as chat).
        if transcript.chars().count() >= 800 {
            let _ = learn::maybe_crystallize_skill(
                provider.clone(),
                model,
                key_for_learn.clone(),
                &transcript,
                800,
            )
            .await;
        }
        for sk in skills {
            if reply.contains(&sk.name) || inbound.text.to_ascii_lowercase().contains(&sk.name) {
                let md = format!(
                    "---\nname: {}\ndescription: {}\n---\n\n{}",
                    sk.name, sk.description, sk.body
                );
                let _ = learn::maybe_improve_skill(
                    provider.clone(),
                    model,
                    key_for_learn.clone(),
                    &sk.name,
                    &md,
                    &transcript,
                    400,
                )
                .await;
            }
        }
    }

    // Collect attachments: explicit attach_file tool, write tool (code mode), fenced files.
    let mut attachments = attach_log.take();
    for p in pirs_claw::attach::paths_from_write_results(&new_msgs) {
        if !attachments.iter().any(|x| x == &p) {
            attachments.push(p);
        }
    }
    if attachments.is_empty() {
        // Fallback: materialize named fenced code blocks as files to send.
        for p in pirs_claw::attach::materialize_fenced_files(&reply, &out_dir) {
            attachments.push(p);
        }
    }

    Ok(GatewayReply {
        text: reply,
        attachments,
    })
}

async fn run_chat(
    state: &Path,
    model: &str,
    cwd: &Path,
    text: &str,
    skills: &[Skill],
    do_learn: bool,
    load_ext: bool,
) -> anyhow::Result<()> {
    let inbound = InboundMessage::cli(text);
    let (provider, key, _reg) = registry::resolve_llm(model, 2)?;
    require_llm_key(key.as_deref())?;
    let key_for_learn = key.clone();
    let host = load_claw_extensions(cwd, load_ext);

    let sid = SessionId::cli_local();
    let store = SessionStore::open_for(state, sid.clone())?;
    store.append("user", text)?;
    let mem = memory_bridge::open_memory(state).ok();
    if let Some(ref m) = mem {
        memory_bridge::scope_session(m, &sid.key());
        memory_bridge::remember_turn(m, "user", text);
    }

    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let mut sys = claw_system_prompt();
    sys.push_str(&skills_prompt_section(skills));
    sys.push_str(&pirs_tools::detect_profile(cwd).prompt_section());
    if let Some(ref m) = mem {
        sys.push_str(&memory_bridge::recall_context(m, text, 5));
    }
    // Cron/heartbeat set PIRS_CLAW_UNATTENDED=1 — never install unrestricted bash
    // unless the operator opts in with PIRS_CLAW_SCHEDULE_CODE=1.
    let mut tools = if pirs_claw::is_unattended() {
        eprintln!("[pirs-claw] unattended tool profile (no bash/write by default)");
        pirs_claw::unattended_tools(cwd)
    } else {
        let mut t = pirs_tools::default_tools(cwd.to_path_buf());
        t.extend(chat_safe_tools(cwd, skills, false, true));
        t
    };
    // Dedupe by name (default_tools may already include recall).
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    let mut agent = Agent::new(provider.clone(), model)
        .with_system_prompt(sys)
        .with_tools(tools)
        .with_completion(completion);
    agent = install_claw_safety(agent, pirs_claw::is_unattended(), host.as_ref());

    let prior = store.load()?;
    if prior.len() > 1 {
        let mut msgs = store.to_agent_messages()?;
        if let Some(pirs_ai::Message::User(_)) = msgs.last() {
            msgs.pop();
        }
        agent.messages = msgs;
    }

    let new_msgs = agent
        .prompt(text)
        .await
        .map_err(|e| anyhow::anyhow!("agent error (no assistant reply recorded): {e}"))?;
    let reply = extract_assistant_reply(&new_msgs).ok_or_else(|| {
        anyhow::anyhow!(
            "empty assistant reply (nothing recorded as assistant; {})",
            empty_assistant_diag(&new_msgs)
        )
    })?;
    store.append("assistant", &reply)?;
    if let Some(ref m) = mem {
        memory_bridge::remember_turn(m, "assistant", &reply);
    }
    if do_learn {
        learn::maybe_memory_nudge(
            provider.clone(),
            model,
            key_for_learn.clone(),
            state,
            &sid.key(),
            text,
            &reply,
        )
        .await;
        let transcript = learn::session_transcript(text, &reply, "");
        let crystallized = learn::maybe_crystallize_skill(
            provider.clone(),
            model,
            key_for_learn.clone(),
            &transcript,
            800,
        )
        .await;
        if crystallized.is_none() {
            // Try improve any installed skill mentioned in the turn.
            for sk in skills {
                if text.to_ascii_lowercase().contains(&sk.name)
                    || reply.to_ascii_lowercase().contains(&sk.name)
                {
                    let md = format!(
                        "---\nname: {}\ndescription: {}\n---\n\n{}",
                        sk.name, sk.description, sk.body
                    );
                    let _ = learn::maybe_improve_skill(
                        provider.clone(),
                        model,
                        key_for_learn.clone(),
                        &sk.name,
                        &md,
                        &transcript,
                        400,
                    )
                    .await;
                }
            }
        }
    }
    CliChannel.deliver(&OutboundReply::to(&inbound, reply))?;
    eprintln!(
        "[pirs-claw chat: session {} exec={}]",
        store.path().display(),
        describe_exec_backend()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // thin CLI wiring; not a public API surface
async fn run_code(
    cwd: &Path,
    model: &str,
    plan_model: &str,
    strategy_name: &str,
    prompt: &str,
    max_turns: Option<usize>,
    sequential: bool,
    skills: &[Skill],
    do_learn: bool,
    load_ext: bool,
) -> anyhow::Result<()> {
    let opts = apply_code_defaults(CodeOptions {
        cwd: cwd.to_path_buf(),
        model: model.into(),
        plan_model: if plan_model.is_empty() {
            None
        } else {
            Some(plan_model.into())
        },
        strategy: strategy_name.into(),
        prompt: Some(prompt.into()),
        max_turns,
        sequential,
    });

    let strategy = resolve_code_strategy(&opts)?;
    eprintln!(
        "[pirs-claw code: cwd={} model={} plan_model={:?} strategy={} phases={} exec={}]",
        opts.cwd.display(),
        opts.model,
        opts.plan_model,
        strategy.name,
        strategy.steps.len(),
        describe_exec_backend()
    );

    let retries = if sequential { 3 } else { 2 };
    let (provider, key, _) = registry::resolve_llm(&opts.model, retries)?;
    require_llm_key(key.as_deref())?;
    let host = load_claw_extensions(&opts.cwd, load_ext);
    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let skill_section = skills_prompt_section(skills);
    let project_section = pirs_tools::detect_profile(&opts.cwd).prompt_section();
    let key_for_learn = completion.api_key.clone();
    let skills_owned: Vec<Skill> = skills.to_vec();
    let host_c = host.clone();

    if strategy.name != "monolithic" && strategy.steps.len() > 1 {
        let opts_c = opts.clone();
        let provider_c = provider.clone();
        let completion_c = completion.clone();
        let skill_section_c = skill_section.clone();
        let project_section_c = project_section.clone();
        let skills_c = skills_owned.clone();
        let mut driver = AgentPhaseDriver::new(move |req: &PhaseReq| {
            let model = req.model.clone().unwrap_or_else(|| opts_c.model.clone());
            let mut tools = coding_tools(&opts_c.cwd);
            tools.extend(chat_safe_tools(&opts_c.cwd, &skills_c, false, true));
            {
                let mut seen = std::collections::HashSet::new();
                tools.retain(|t| seen.insert(t.name().to_string()));
            }
            if req.scope == ToolScope::ReadOnly {
                tools.retain(|t| {
                    matches!(
                        t.name(),
                        "read"
                            | "grep"
                            | "find"
                            | "ls"
                            | "code_map"
                            | "code_search"
                            | "recall"
                            | "skill_list"
                            | "skill_view"
                            | "web_fetch"
                            | "web_search"
                            | "project"
                            | "run_tests"
                    )
                });
            }
            let mut system = if req.system.trim().is_empty() {
                coding_system_prompt(&opts_c.cwd)
            } else {
                req.system.clone()
            };
            system.push_str(&skill_section_c);
            system.push_str(&project_section_c);
            let cwd_for_sub = opts_c.cwd.clone();
            let sub = pirs_agent::delegate::DelegateTool::new(
                provider_c.clone(),
                opts_c.model.clone(),
                completion_c.clone(),
                move || coding_tools(&cwd_for_sub),
            );
            tools.push(sub);
            let mut agent = Agent::new(provider_c.clone(), model)
                .with_system_prompt(system)
                .with_tools(tools)
                .with_completion(completion_c.clone());
            agent = install_claw_safety(agent, false, host_c.as_ref());
            if let Some(n) = opts_c.max_turns {
                agent.budgets.max_turns = Some(n);
            }
            if opts_c.sequential {
                agent = agent.with_tool_execution(pirs_agent::ExecutionMode::Sequential);
            }
            agent
        });

        let task = Task {
            issue: prompt.to_string(),
            targets: Vec::new(),
            verdict: None,
        };
        run_strategy_async(&strategy, &mut driver, &task).await?;
        let reply = extract_assistant_reply(driver.messages())
            .unwrap_or_else(|| "(strategy completed; no final assistant text)".into());
        if do_learn {
            let transcript = learn::session_transcript(prompt, &reply, "code strategy run");
            let _ = learn::maybe_crystallize_skill(
                provider,
                model,
                key_for_learn,
                &transcript,
                400,
            )
            .await;
        }
        println!("{reply}");
        return Ok(());
    }

    let mut sys = coding_system_prompt(&opts.cwd);
    sys.push_str(&skill_section);
    sys.push_str(&project_section);
    let cwd_for_sub = opts.cwd.clone();
    let sub = pirs_agent::delegate::DelegateTool::new(
        provider.clone(),
        opts.model.clone(),
        completion.clone(),
        move || coding_tools(&cwd_for_sub),
    );
    let mut tools = coding_tools(&opts.cwd);
    tools.extend(chat_safe_tools(&opts.cwd, skills, false, true));
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    tools.push(sub);
    let mut agent = build_code_agent(provider.clone(), &opts)
        .with_completion(completion)
        .with_system_prompt(sys)
        .with_tools(tools);
    agent = install_claw_safety(agent, false, host.as_ref());
    let msgs = agent.prompt(prompt).await?;
    if let Some(reply) = extract_assistant_reply(&msgs) {
        if do_learn {
            let transcript = learn::session_transcript(prompt, &reply, "code run");
            let _ = learn::maybe_crystallize_skill(
                provider,
                model,
                key_for_learn,
                &transcript,
                400,
            )
            .await;
        }
        println!("{reply}");
    } else {
        anyhow::bail!(
            "empty assistant reply ({})",
            empty_assistant_diag(&msgs)
        );
    }
    Ok(())
}
