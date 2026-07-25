//! pirs-claw — Hermes-class agent (local/docker/ssh; multi-channel gateway).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
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

mod bin_helpers;
mod cli;

use bin_helpers::*;
use cli::{
    BlueprintCmd, Cli, Commands, PairCmd, ScheduleCmd, SessionsCmd, SkillsCmd, SoulCmd, SpeechCmd,
};

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
                    let peer = pirs_claw::pairing::normalize_peer_id(&peer);
                    let added = al.add(&path, &peer)?;
                    if added {
                        println!("paired {peer} → {}", path.display());
                        println!(
                            "tip: or mint a code with `pirs-claw pair code` and have them DM it"
                        );
                    } else {
                        println!("already paired: {peer}");
                    }
                }
                PairCmd::Remove { peer } => {
                    let peer = pirs_claw::pairing::normalize_peer_id(&peer);
                    let removed = al.remove(&path, &peer)?;
                    if removed {
                        println!("unpaired {peer}");
                    } else {
                        println!("not in allowlist: {peer}");
                    }
                }
                PairCmd::Code { ttl } => {
                    let code = pirs_claw::pairing::mint_pairing_code(&state, ttl)?;
                    println!("pairing code: {code}");
                    println!(
                        "unpaired peers: DM this code to the bot within {ttl}s to self-pair"
                    );
                    println!("allowlist file: {}", path.display());
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

