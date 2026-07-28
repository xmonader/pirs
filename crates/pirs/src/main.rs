use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use clap::{CommandFactory, FromArgMatches, Parser};
use pirs_agent::{Agent, AgentEvent, AgentTool, Hooks};
use pirs_ai::{CompletionOptions, Message, OpenAiCompat};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

mod acp_mode;
mod approval;
mod runtime_features;
mod runtime_safety;
mod auth;
mod blame;
mod cli;
mod config_file;
mod discovery;
mod gates;
mod login;
mod pack;
mod printer;
mod replay;
mod repl;
mod rpc_mode;
mod serve;
mod session;
mod subagent;
mod system_prompt;
mod tui;
mod turn;
mod observability;
mod models_cmd;
mod registry;
mod secrets_edit;
mod session_stats;
mod weak_compose;

use cli::Cli;
use gates::{
    chain_gate_with_extensions, install_gate_if_absent, install_profile_under_yolo_if_needed,
    summarize_args,
};
use login::parse_login_request;
use printer::Printer;
use repl::repl;
use turn::run_turn;

pub(crate) use turn::run_strategy_turn;

/// Serializes tests that mutate process-global env (HOME, secrets path, etc.).
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Parsed via ArgMatches (rather than plain `Cli::parse()`) so
    // `value_source()` can tell a value the user actually typed/exported
    // apart from one that just fell through to clap's hardcoded default —
    // that distinction is what lets project/user config.toml layers fill in
    // underneath CLI/env without ever overriding something the user set.
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    // Flatten trailing args into the single prompt string the rest of main
    // (and the pseudo-subcommands) expect.
    let mut cli = Cli {
        prompt: {
            let parts = std::mem::take(&mut cli.prompt);
            if parts.is_empty() {
                Vec::new()
            } else {
                vec![parts.join(" ")]
            }
        },
        ..cli
    };
    // --yolo is a first-class full-autonomy switch (must run before show-config
    // and before config layering so approval is not left at default "auto").
    if cli.yolo {
        cli.approval = "yolo".into();
        if cli.autonomy.is_none() {
            cli.autonomy = Some("full".into());
        }
    }

    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Capture --also paths before chdir (they may be relative to launch dir).
    let also_dirs: Vec<PathBuf> = cli
        .also
        .iter()
        .map(|d| {
            if d.is_absolute() {
                d.clone()
            } else {
                cwd.join(d)
            }
        })
        .collect();
    // Optional project directory (before config/tools resolve).
    if let Some(ref dir) = cli.cwd.clone() {
        let abs = if dir.is_absolute() {
            dir.clone()
        } else {
            cwd.join(dir)
        };
        let abs = abs
            .canonicalize()
            .with_context(|| format!("--cwd {}: path not found", dir.display()))?;
        if !abs.is_dir() {
            anyhow::bail!("--cwd {}: not a directory", abs.display());
        }
        std::env::set_current_dir(&abs)
            .with_context(|| format!("--cwd {}: set_current_dir failed", abs.display()))?;
        cwd = abs;
        eprintln!("[cwd: {}]", cwd.display());
    }
    // Optional git worktree bind (Vibe --worktree class) before tools use cwd.
    if let Some(ref wt) = cli.worktree.clone() {
        match pirs_tools::bind_session_worktree(&cwd, &wt) {
            Ok(sess) => {
                eprintln!(
                    "[worktree: branch={} cwd={} created={}]",
                    sess.branch,
                    sess.cwd.display(),
                    sess.created
                );
                if let Err(e) = std::env::set_current_dir(&sess.cwd) {
                    eprintln!("[worktree: set_current_dir failed: {e}]");
                } else {
                    cwd = sess.cwd;
                }
            }
            Err(e) => {
                anyhow::bail!("--worktree {wt:?}: {e}");
            }
        }
    }

    // Multi-root work context: primary = cwd, plus --also / --context roots.
    {
        let mut ctx = if let Some(ref name) = cli.context {
            pirs_tools::load_named_context(name)
                .with_context(|| format!("--context {name}"))?
        } else {
            pirs_tools::WorkContext::single(cwd.clone())
        };
        if !also_dirs.is_empty() {
            // Merge --also into context (primary stays first from context or cwd).
            let primary = ctx.primary.clone();
            let mut extra: Vec<PathBuf> = ctx
                .roots
                .iter()
                .skip(1)
                .map(|r| r.path.clone())
                .collect();
            extra.extend(also_dirs);
            ctx = pirs_tools::WorkContext::from_paths(primary, extra)?;
        } else if cli.context.is_some() {
            // Named context may point primary elsewhere — chdir to it.
            if ctx.primary != cwd {
                if let Err(e) = std::env::set_current_dir(&ctx.primary) {
                    eprintln!("[context: set_current_dir {}: {e}]", ctx.primary.display());
                } else {
                    cwd = ctx.primary.clone();
                }
            }
        }
        // Ensure primary matches process cwd after all chdirs.
        if ctx.primary != cwd {
            ctx.primary = cwd.clone();
            if let Some(r) = ctx.roots.first_mut() {
                r.path = cwd.clone();
                if r.name.is_empty() {
                    r.name = cwd
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("root")
                        .into();
                }
            }
        }
        pirs_tools::install_work_context(ctx.clone());
        if ctx.roots.len() > 1 {
            eprintln!("[{}]", ctx.summary_line());
        }
    }
    let (project_cfg, user_cfg) = crate::config_file::load_layers(&cwd);
    // `base_url`/`approval` are security-relevant (redirect API traffic /
    // disable the approval gate) so they are deliberately NEVER read from the
    // project layer — a `git clone`d repo's own .pirs/config.toml must not be
    // able to silently point requests at an attacker's endpoint or turn off
    // approval prompts just by being checked out. model/provider are inert
    // preferences and stay project-configurable. See crate::config_file::FileConfig.
    if project_cfg.base_url.is_some() || project_cfg.approval.is_some() {
        eprintln!(
            "[note: project .pirs/config.toml sets base_url/approval, which are user-config-only and were ignored]"
        );
    }
    let project_cfg = crate::config_file::restrict_project_layer(project_cfg);
    let (model, model_src) = crate::config_file::resolve_str(
        &matches,
        "model",
        &cli.model,
        project_cfg.model.as_deref(),
        user_cfg.model.as_deref(),
    );
    let (provider, provider_src) = crate::config_file::resolve_str(
        &matches,
        "provider",
        &cli.provider,
        project_cfg.provider.as_deref(),
        user_cfg.provider.as_deref(),
    );
    let (base_url, base_url_src) = crate::config_file::resolve_opt(
        &matches,
        "base_url",
        cli.base_url.clone(),
        project_cfg.base_url.as_deref(),
        user_cfg.base_url.as_deref(),
    );
    let (mut approval, mut approval_src) = crate::config_file::resolve_str(
        &matches,
        "approval",
        &cli.approval,
        project_cfg.approval.as_deref(),
        user_cfg.approval.as_deref(),
    );
    if cli.yolo {
        approval = "yolo".into();
        approval_src = crate::config_file::ConfigSource::Cli;
    }
    if cli.show_config {
        let autonomy = pirs_tools::resolve_autonomy(
            cli.mode_dial.as_deref(),
            cli.autonomy.as_deref(),
            cli.tool_preset.as_deref(),
            cli.permission_mode.as_deref(),
            &approval,
            &cli.agent_profile,
        );
        println!("model:      {model:<24} ({})", model_src.label());
        println!("provider:   {provider:<24} ({})", provider_src.label());
        println!(
            "base_url:   {:<24} ({})",
            base_url.as_deref().unwrap_or("(none)"),
            base_url_src.label()
        );
        println!("approval:   {approval:<24} ({})", approval_src.label());
        println!("{}", pirs_tools::autonomy_status_line(autonomy));
        return Ok(());
    }
    if cli.doctor {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        for line in pirs_tools::doctor_report(&cwd) {
            println!("{line}");
        }
        return Ok(());
    }
    let mut cli = Cli {
        model,
        provider,
        base_url,
        approval,
        ..cli
    };
    // --weak: compose the recommended weak-model preset without requiring the
    // user to remember every flag. Pure rules live in weak_compose (unit-tested).
    if cli.weak {
        let detected = if cli.verify.is_none() && !cli.prompt.is_empty() {
            pirs_tools::run_tests::detect_verify_command(&cwd)
        } else {
            None
        };
        let composed = crate::weak_compose::apply_weak_preset(
            crate::weak_compose::WeakComposeInput {
                has_prompt: !cli.prompt.is_empty(),
                strategy: cli.strategy.clone(),
                profile: cli.profile.clone(),
                verify: cli.verify.clone(),
                max_retries: cli.max_retries,
                tool_diet: cli.tool_diet,
                sequential: cli.sequential,
            },
            detected,
        );
        cli.tool_diet = composed.tool_diet;
        cli.sequential = composed.sequential;
        cli.max_retries = composed.max_retries;
        cli.strategy = composed.strategy;
        cli.profile = composed.profile;
        cli.verify = composed.verify;
        if let Some(note) = &composed.auto_verify_note {
            eprintln!("{note}");
        }
        eprintln!(
            "[weak mode: tool-diet, sequential, max-retries={}, strategy={:?}, \
             verify={:?}; packs stay on profile default (*); multi-model: phase \
             `model:` in strategies and/or --cascade <draft>]",
            cli.max_retries,
            cli.strategy.as_deref().or(cli.profile.as_deref()),
            cli.verify.as_deref(),
        );
    }

    // Model/backends inspection (no API key required for listing).
    if let Some(spec) = cli.prompt.first().cloned() {
        if crate::models_cmd::try_run(&cwd, &spec)? {
            return Ok(());
        }
    }

    if let Some(dir) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "trust" || p.starts_with("trust "))
    {
        let arg = dir.trim_start_matches("trust").trim().to_string();
        let target = if arg.is_empty() {
            std::env::current_dir()?
        } else {
            std::path::PathBuf::from(arg)
        };
        return match pirs_rhai::trust_directory(&target) {
            Ok(()) => {
                println!("trusted {}", target.display());
                Ok(())
            }
            Err(e) => anyhow::bail!(e),
        };
    }
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "replay" || p.starts_with("replay "))
    {
        // pirs replay <session.jsonl> [--model X]
        let args: Vec<&str> = spec
            .trim_start_matches("replay")
            .split_whitespace()
            .collect();
        let Some(file) = args.first().copied() else {
            anyhow::bail!("usage: pirs replay <session.jsonl> [--model X]");
        };
        let live_model = args
            .windows(2)
            .find(|w| w[0] == "--model")
            .map(|w| w[1].to_string());
        let tape = crate::replay::load_cassette(std::path::Path::new(file))?;
        let cwd = std::env::current_dir()?;
        let diverged = std::sync::Arc::new(std::sync::Mutex::new(None));
        let live = live_model.is_some();

        let model = live_model.clone().unwrap_or_else(|| "replay".to_string());
        let provider: Arc<dyn pirs_ai::LlmProvider> = if live {
            if cli.provider == "anthropic" {
                Arc::new(pirs_ai::AnthropicClient::new(cli.base_url.clone()))
            } else {
                Arc::new(OpenAiCompat::new(cli.base_url.clone()))
            }
        } else {
            Arc::new(crate::replay::ReplayProvider::new(&tape))
        };
        let tools: Vec<Arc<dyn pirs_agent::AgentTool>> = pirs_tools::default_tools(cwd.clone())
            .into_iter()
            .map(|t| {
                Arc::new(crate::replay::CassetteTool::wrap(
                    t,
                    &tape,
                    live,
                    std::sync::Arc::clone(&diverged),
                )) as Arc<dyn pirs_agent::AgentTool>
            })
            .collect();
        // Live replay calls the real LLM, so it must see the same system
        // prompt the recording used — the default placeholder would produce a
        // different session and every message would spuriously "diverge".
        // (Strict replay drives the model from the tape, so the prompt is
        // inert there; building it unconditionally keeps one code path.)
        let mut system = crate::system_prompt::build_system_prompt(&cwd, &tools);
        if let Some(ctx) = crate::system_prompt::read_project_context(&cwd) {
            system.push_str(&ctx);
        }
        let mut agent = Agent::new(provider, &model)
            .with_system_prompt(system)
            .with_tools(tools);
        let produced = crate::replay::run_replay(&mut agent, &tape).await;
        let report = crate::replay::compare(&crate::replay::expected_of(&tape), &produced);
        match report.divergence {
            None => {
                println!("replay: {} messages matched", report.matched);
                return Ok(());
            }
            Some(d) => {
                eprintln!(
                    "replay diverged at message {} ({}): expected {}, got {}",
                    d.index, d.kind, d.expected, d.actual
                );
                if let Some(t) = diverged.lock().unwrap().as_ref() {
                    eprintln!("first tool divergence: {t}");
                }
                std::process::exit(if live { 2 } else { 1 });
            }
        }
    }
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p.starts_with("pack install "))
    {
        // pirs pack install <git-url> [--pin <ref>] [--yes] [--force]
        let args: Vec<&str> = spec
            .trim_start_matches("pack install ")
            .split_whitespace()
            .collect();
        let Some(url) = args.first().copied() else {
            anyhow::bail!("usage: pirs pack install <git-url> [--pin <ref>] [--yes] [--force]");
        };
        let flag = |name: &str| {
            args.windows(2)
                .find(|w| w[0] == name)
                .map(|w| w[1].to_string())
        };
        let pin = flag("--pin");
        let yes = args.contains(&"--yes");
        let force = args.contains(&"--force");

        let name = crate::pack::pack_name_from_url(url);
        eprintln!(
            "[pack: cloning {url}{}]",
            pin.as_deref()
                .map(|p| format!(" @ {p}"))
                .unwrap_or_default()
        );
        let (tmp, head) = crate::pack::clone_pinned(url, pin.as_deref())?;
        let scripts = crate::pack::collect_scripts(&tmp.path().join("repo"));
        if scripts.is_empty() {
            anyhow::bail!("{url}: no .rhai scripts found (root, extensions/, packs/)");
        }
        println!("pack {name} @ {head} ({} scripts):", scripts.len());
        for s in &scripts {
            let src = std::fs::read_to_string(s).unwrap_or_default();
            println!(
                "  {}: {}",
                s.file_name().and_then(|f| f.to_str()).unwrap_or("?"),
                pirs_rhai::caps::parse_caps(&src).summary()
            );
        }
        if !yes {
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                anyhow::bail!("refusing to install without confirmation (pass --yes)");
            }
            eprint!("install into ~/.pirs/packs? [y/N] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !matches!(line.trim(), "y" | "yes" | "Y") {
                anyhow::bail!("aborted");
            }
        }
        let home = std::env::var("HOME").context("HOME not set")?;
        // Installed packs go to ~/.pirs/packs, which is trust-gated (hash-bound)
        // on load — NOT ~/.pirs/extensions, which is auto-run. So remote code
        // can't execute just by landing on disk: the first run prompts (showing
        // caps) and a later tamper/pull re-prompts. --yes skips the *install*
        // confirmation only, never the load-time trust decision.
        let dest = std::path::Path::new(&home).join(".pirs").join("packs");
        let installed = crate::pack::install_scripts(&scripts, &dest, force)?;
        for p in &installed {
            println!("installed {}", p.display());
        }
        println!(
            "note: installed packs are trust-gated; the next `pirs` run will \
             ask to trust ~/.pirs/packs before loading them."
        );
        return Ok(());
    }
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "blame" || p.starts_with("blame "))
    {
        let arg = spec.trim_start_matches("blame").trim().to_string();
        let Some((file, line)) = arg.rsplit_once(':') else {
            anyhow::bail!("usage: pirs blame <file>:<line>");
        };
        let line: u32 = line.parse().context("line must be a number")?;
        let cwd = std::env::current_dir()?;
        let info = crate::blame::blame_line(&cwd, file, line)?;
        println!("{}", crate::blame::format_blame(&info));
        return Ok(());
    }
    // Deterministic code review: file selection + units + structured findings (no LLM).
    // Usage: pirs review [--json] [--from REV] [--to REV] [--no-untracked]
    if let Some(spec) = cli
        .prompt
        .first()
        .cloned()
        .filter(|p| p == "review" || p.starts_with("review "))
    {
        let rest: Vec<&str> = spec
            .trim_start_matches("review")
            .split_whitespace()
            .collect();
        if rest.iter().any(|a| *a == "-h" || *a == "--help") {
            println!(
                "usage: pirs review [--json] [--from REV] [--to REV] [--no-untracked] [--cargo-check]\n\
                 \n\
                 Deterministic review plan over git changes (model does not choose files).\n\
                 Emits structured findings (JSON with --json). Read-only tool diet only.\n\
                 --cargo-check runs cargo check (JSON) when Cargo.toml is present."
            );
            return Ok(());
        }
        let opts = pirs_tools::code_review::parse_review_cli_args(&rest);
        let as_json = pirs_tools::code_review::wants_json(&rest);
        let cwd = std::env::current_dir()?;
        let report = pirs_tools::run_review(&cwd, &opts)?;
        if as_json {
            println!("{}", pirs_tools::format_report_json(&report)?);
        } else {
            println!("{}", pirs_tools::format_report_text(&report));
        }
        return Ok(());
    }
    let cwd = std::env::current_dir()?;

    if let Some(provider) =
        parse_login_request(cli.prompt.first().map(|s| s.as_str()), &cli.mode, &cli.provider)
    {
        return crate::auth::login(provider);
    }

    // Load ~/.pirs/secrets.env into process env (does not override existing vars).
    crate::registry::load_secrets_env();
    // Model registry first so backend api_key_env can satisfy auth without OPENAI_API_KEY.
    let model_registry = crate::registry::load_registry_layers(&cwd);

    let env_var = if cli.provider == "anthropic" {
        "ANTHROPIC_API_KEY"
    } else {
        "OPENAI_API_KEY"
    };
    // Model-aware OpenAI-compat env fallback (DASHSCOPE/DEEPSEEK/OPENROUTER) so a
    // missing registry or empty OPENAI_API_KEY still works with secrets.env keys.
    let (compat_base, compat_key) = if cli.provider == "anthropic" {
        (None, None)
    } else {
        pirs_ai::resolve_openai_compat(Some(&cli.model))
    };
    let api_key = crate::auth::resolve(cli.api_key.as_deref(), &cli.provider, env_var)
        .or_else(|| crate::registry::api_key_for_alias(&model_registry, &cli.model))
        .or_else(|| {
            cli.plan_model
                .as_ref()
                .and_then(|m| crate::registry::api_key_for_alias(&model_registry, m))
        })
        .or_else(|| crate::registry::first_available_backend_key(&model_registry))
        .or(compat_key)
        .with_context(|| {
            let mut hint = format!(
                "no API key: pass --api-key, run `pirs login`, set {env_var}"
            );
            let mut envs = crate::registry::expected_key_envs(&model_registry);
            for k in pirs_ai::well_known_key_envs() {
                if !envs.iter().any(|e| e == k) {
                    envs.push((*k).to_string());
                }
            }
            if !envs.is_empty() {
                hint.push_str(&format!(" (also tried {})", envs.join(" / ")));
            }
            hint.push_str(" — ensure ~/.pirs/secrets.env is loaded (HOME must point at your user home)");
            hint
        })?;

    // When the user did not pin --base-url / config base_url, use the endpoint
    // that matches the env key we resolved (deepseek vs dashscope vs …).
    if cli.base_url.is_none() {
        if let Some(b) = compat_base {
            cli.base_url = Some(b);
        }
    }

    if cli.mode == "rpc" {
        return crate::rpc_mode::run(crate::rpc_mode::RpcOptions {
            cwd: cwd.clone(),
            model: cli.model.clone(),
            base_url: cli.base_url.clone(),
            api_key,
            max_retries: cli.max_retries,
            provider: cli.provider.clone(),
            approval: cli.approval.clone(),
            agent_profile: cli.agent_profile.clone(),
            permission_mode: cli.permission_mode.clone(),
        })
        .await;
    }
    if cli.mode == "acp" {
        return crate::acp_mode::run(crate::acp_mode::AcpOptions {
            cwd: cwd.clone(),
            model: cli.model.clone(),
            base_url: cli.base_url.clone(),
            api_key,
            max_retries: cli.max_retries,
            provider: cli.provider.clone(),
            approval: cli.approval.clone(),
            agent_profile: cli.agent_profile.clone(),
            permission_mode: cli.permission_mode.clone(),
        })
        .await;
    }
    if !matches!(cli.mode.as_str(), "repl" | "tui" | "web") {
        bail!("unknown mode: {} (expected repl|tui|web|rpc|acp)", cli.mode);
    }

    let default_provider: Arc<dyn pirs_ai::LlmProvider> = if cli.provider == "anthropic" {
        Arc::new(
            pirs_ai::AnthropicClient::new(cli.base_url.clone()).with_max_retries(cli.max_retries),
        )
    } else if cli.provider == "openai" {
        Arc::new(
            OpenAiCompat::new(cli.base_url.clone())
                .with_max_retries(cli.max_retries)
                .with_cache_key(format!("pirs-{}-{}", std::process::id(), cli.model)),
        )
    } else {
        anyhow::bail!(
            "unknown provider '{}' (expected openai|anthropic)",
            cli.provider
        );
    };
    // Multi-backend registry: pin `backend/model` or portable bare names.
    // Builtins + user config; see `pirs backends` / `pirs models`.
    let provider: Arc<dyn pirs_ai::LlmProvider> =
        if let Some(router) = crate::registry::build_routing_provider(
            &model_registry,
            Arc::clone(&default_provider),
            Some(api_key.clone()),
            cli.max_retries,
        )? {
            let active_n = pirs_ai::active_backends(&model_registry).len();
            let portable: Vec<_> = pirs_ai::active_portable_models(&model_registry)
                .into_iter()
                .map(|m| m.alias.as_str())
                .take(12)
                .collect();
            eprintln!(
                "[model registry: {} backend(s), {} with keys; portable: {}{} — pin with backend/model]",
                model_registry.backends.len(),
                active_n,
                portable.join(", "),
                if pirs_ai::active_portable_models(&model_registry).len() > 12 {
                    ", …"
                } else {
                    ""
                }
            );
            router
        } else {
            default_provider
        };
    let usage_slot: std::sync::Arc<std::sync::Mutex<pirs_ai::Usage>> =
        std::sync::Arc::new(std::sync::Mutex::new(pirs_ai::Usage::default()));

    let mut tools: Vec<Arc<dyn AgentTool>> = pirs_tools::default_tools(cwd.clone());
    let mut hooks = Hooks::default();

    // Named tool-policy presets only apply experiment knobs (diet/sequential/budget).
    // Autonomy (tool access) is resolved next as a single ladder.
    if let Some(raw) = cli.tool_preset.as_deref() {
        let preset = pirs_tools::ToolPreset::parse(raw).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown --tool-preset {raw:?}; expected full|edit-test|read-only|no-tools"
            )
        })?;
        pirs_tools::apply_tool_preset(
            preset,
            &mut cli.permission_mode,
            &mut cli.agent_profile,
            &mut cli.tool_diet,
            &mut cli.sequential,
            &mut cli.max_tool_calls,
        );
        eprintln!(
            "[tool-preset: {} — mutation={} edit+test-loop={}]",
            preset.name(),
            preset.allows_mutation(),
            preset.allows_edit_and_test_loop()
        );
    }

    // ── One autonomy ladder (plan | edit | full) ───────────────────────────
    // Collapses --yolo / --autonomy / --mode-dial / --tool-preset /
    // --permission-mode / --approval / --agent-profile into one product level.
    // (--yolo already applied at parse time → approval=yolo, autonomy=full.)
    if let Some(d) = cli.mode_dial.as_deref() {
        let norm = d.trim().to_ascii_lowercase();
        if !matches!(norm.as_str(), "plan" | "act" | "edit" | "full" | "yolo") {
            bail!("unknown --mode-dial {d:?}; expected plan|act|edit|full");
        }
    }
    let autonomy = pirs_tools::resolve_autonomy(
        cli.mode_dial.as_deref(),
        cli.autonomy.as_deref(),
        cli.tool_preset.as_deref(),
        cli.permission_mode.as_deref(),
        &cli.approval,
        &cli.agent_profile,
    );
    // Expand autonomy → permission + profile + approval (single write path).
    pirs_tools::apply_autonomy(autonomy);
    let safety = autonomy.profile();
    let perm_mode = autonomy.permission();
    // Approval: explicit CLI wins if parseable; else autonomy default (full→yolo).
    let approval_mode = crate::approval::ApprovalMode::parse(&cli.approval)
        .or_else(|| crate::approval::ApprovalMode::parse(autonomy.approval_name()))
        .unwrap_or(crate::approval::ApprovalMode::Auto);
    // If user said --approval yolo but autonomy resolved lower via explicit pin,
    // keep their approval for prompts while permission stays pinned.
    let approval_mode = if cli.approval.eq_ignore_ascii_case("yolo") {
        crate::approval::ApprovalMode::Yolo
    } else if autonomy.is_yolo() && cli.approval.eq_ignore_ascii_case("auto") {
        crate::approval::ApprovalMode::Yolo
    } else {
        approval_mode
    };
    eprintln!("[{}]", pirs_tools::autonomy_status_line(autonomy));
    // Always install gate when a non-default safety profile is set (hard denials),
    // or when approval is Ask. Auto+default stays open.
    let gate = std::sync::Arc::new(crate::approval::ApprovalGate::with_profile(
        approval_mode,
        cwd.clone(),
        safety,
    ));
    let mut gate_hook = if approval_mode == crate::approval::ApprovalMode::Ask
        || safety != pirs_tools::SafetyProfile::Default
    {
        Some(gate.hook())
    } else {
        None
    };
    // Permission ladder always installed; re-reads live mode for /plan|/act|/yolo.
    {
        let ph = pirs_tools::live_permission_hook();
        gate_hook = pirs_agent::Hooks::chain_before(gate_hook, Some(ph));
    }
    let _ = perm_mode;

    // Semantic search needs the vector store, so it implies the persistent graph.
    let graph_db = cwd.join(".pirs").join("graph.db");
    let graph: Option<std::sync::Arc<pirs_graph::LazyGraph>> = if cli.no_graph {
        None
    } else if cli.persist_graph || cli.semantic {
        Some(std::sync::Arc::new(pirs_graph::LazyGraph::persistent(
            cwd.clone(),
            graph_db.clone(),
        )))
    } else {
        Some(std::sync::Arc::new(pirs_graph::LazyGraph::new(cwd.clone())))
    };
    let mut sub_tools = tools.clone();
    if let Some(g) = &graph {
        let map_tool = std::sync::Arc::new(pirs_graph::code_map::CodeMapTool::new(
            std::sync::Arc::clone(g),
            cwd.clone(),
        ));
        let ast_tool = std::sync::Arc::new(pirs_graph::ast_edit::AstEditTool::new(cwd.clone()));
        tools.push(map_tool.clone());
        tools.push(ast_tool.clone());
        sub_tools.push(map_tool);
        sub_tools.push(ast_tool);

        // The optional semantic arm of code_search. BM25 + graph work with no
        // model; embeddings are added only when --semantic supplies one.
        let embedder = if cli.semantic {
            match cli.embed_model.clone() {
                Some(model) => {
                    let base = cli
                        .embed_base_url
                        .clone()
                        .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
                    Some(pirs_ai::EmbeddingClient::new(
                        base,
                        model,
                        cli.embed_api_key.clone(),
                    ))
                }
                None => {
                    eprintln!(
                        "warning: --semantic requires --embed-model (or PIRS_EMBED_MODEL); \
                         code_search will run lexical+graph only"
                    );
                    None
                }
            }
        } else {
            None
        };
        // Indexing strategy:
        //  - default: a background indexer fills the embedding index in
        //    checkpointed batches, and code_search is query-only (embed_cap = 0)
        //    so a search NEVER blocks on embedding — BM25 answers instantly and
        //    semantic hits light up as vectors land.
        //  - --embed-batch-cap N: opt into synchronous inline indexing instead
        //    (code_search embeds up to N symbols per call, no background task) —
        //    useful for a one-shot where you want the index built in-process.
        let inline = cli.embed_batch_cap.is_some();
        let code_cap = if embedder.is_some() {
            if inline {
                cli.embed_batch_cap
            } else {
                Some(0)
            }
        } else {
            None
        };
        if let (Some(emb), false) = (&embedder, inline) {
            let bg = pirs_graph::BackgroundIndexer::new(
                cwd.clone(),
                graph_db.clone(),
                emb.clone(),
                cli.embed_max_chars.unwrap_or(2000),
            );
            tokio::spawn(bg.run());
        }
        // Reused for `recall`'s semantic mode below — cloned before the move
        // into CodeSearchTool::new, since both tools want the same embedder
        // rather than each constructing (and each paying for) their own.
        let recall_embedder = embedder.clone();
        let code_search = std::sync::Arc::new(pirs_graph::code_search::CodeSearchTool::new(
            std::sync::Arc::clone(g),
            cwd.clone(),
            graph_db.clone(),
            embedder,
            cli.embed_max_chars,
            code_cap,
        ));
        tools.push(code_search.clone());
        sub_tools.push(code_search);

        // Upgrades the bare `recall` tool already in `tools` (from
        // default_tools()) to one that also supports `mode: "semantic"` —
        // the later-registered tool with the same name wins on dispatch, so
        // this doesn't need to remove the earlier one.
        if let Some(emb) = recall_embedder {
            let recall = std::sync::Arc::new(pirs_tools::RecallTool::with_embedder(emb));
            tools.push(recall.clone());
            sub_tools.push(recall);
        }
    }
    {
        let manifests = [
            "Cargo.toml",
            "package.json",
            "go.mod",
            "pyproject.toml",
            "setup.py",
        ];
        let has_project = manifests.iter().any(|m| cwd.join(m).exists());
        let has_server = pirs_lsp::client::SERVERS
            .iter()
            .any(pirs_lsp::client::server_available);
        if has_project && has_server {
            let found: Vec<&str> = pirs_lsp::client::SERVERS
                .iter()
                .filter(|s| pirs_lsp::client::server_available(s))
                .map(|s| s.language)
                .collect();
            eprintln!("[lsp: {}]", found.join(", "));
            let lsp_tool = std::sync::Arc::new(pirs_lsp::tool::LspTool::new(cwd.clone()));
            tools.push(lsp_tool.clone());
            sub_tools.push(lsp_tool);
            // Compound rename: one call rewrites a symbol across the project via
            // the language server's own reference analysis.
            let rename_tool =
                std::sync::Arc::new(pirs_lsp::rename::RenameSymbolTool::new(cwd.clone()));
            tools.push(rename_tool.clone());
            sub_tools.push(rename_tool);
        }
    }
    let mut policy_hooks: Option<(
        pirs_agent::events::BeforeToolCallHook,
        pirs_agent::events::AfterToolCallHook,
    )> = None;
    let policy_slot: std::sync::Arc<
        std::sync::Mutex<
            Option<(
                pirs_agent::events::BeforeToolCallHook,
                pirs_agent::events::AfterToolCallHook,
            )>,
        >,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));

    let host = if cli.no_extensions {
        None
    } else {
        pirs_rhai::register_core_host_apis();
        let mut h = pirs_rhai::ExtensionHost::new();
        if let Some(g) = &graph {
            let g = std::sync::Arc::clone(g);
            let cwd_q = cwd.clone();
            pirs_rhai::register_query_fn("graph_affected_tests", move |path| {
                let p = std::path::PathBuf::from(path);
                let abs = if p.is_absolute() { p } else { cwd_q.join(p) };
                g.get().affected_tests(&abs)
            });
        }
        h.set_subagent_runner(crate::subagent::build_subagent_runner(
            std::sync::Arc::clone(&provider),
            CompletionOptions {
                api_key: Some(api_key.clone()),
                ..Default::default()
            },
            cli.model.clone(),
            sub_tools.clone(),
            std::sync::Arc::clone(&policy_slot),
            std::sync::Arc::clone(&usage_slot),
        ));
        // Catalog packs come from the pack profile (`default` or `--profile`).
        // Project/user dirs load after so last-wins overrides.
        match pirs_rhai::discover::resolve_pack_profile(cli.profile.as_deref(), &cwd) {
            Ok(pack_profile) => {
                let packs = pack_profile.packs.as_deref();
                pirs_rhai::weak_packs::load_profile_packs(&mut h, packs);
                let stems = pirs_rhai::weak_packs::effective_pack_stems(packs);
                if !stems.is_empty() {
                    let summary = match packs {
                        None => format!(
                            "inherit default * ({} packs)",
                            pirs_rhai::weak_packs::BUNDLED_ORDER.len()
                        ),
                        Some(p) if p.iter().any(|s| s == "*" || s == "all") => {
                            format!("* ({} packs)", pirs_rhai::weak_packs::BUNDLED_ORDER.len())
                        }
                        Some(p) => p.join(", "),
                    };
                    eprintln!("[profile packs: {} · {}]", pack_profile.name, summary);
                }
            }
            Err(e) => {
                eprintln!("[profile packs: failed to resolve ({e:#}); loading full catalog]");
                pirs_rhai::weak_packs::load_into(&mut h);
            }
        }
        h.load_default_dirs(&cwd);
        for err in &h.load_errors {
            eprintln!("[extension error] {err}");
        }
        let h = Arc::new(h);
        if !h.extension_names().is_empty() {
            eprintln!("[extensions: {}]", h.extension_names().join(", "));
        }
        tools.extend(h.tools());
        let ext_hooks = h.hooks();
        let yolo =
            crate::approval::ApprovalMode::parse(&cli.approval) == Some(crate::approval::ApprovalMode::Yolo);
        // Subagents inherit gate+extension policy. Previously required BOTH
        // before and after hooks, so packs with only on_tool_call (strict-plan,
        // session-discipline, weak-model) never reached subagents.
        let chained_before = chain_gate_with_extensions(
            gate_hook.clone(),
            ext_hooks.before_tool_call.clone(),
            yolo,
            safety,
        );
        let after_for_sub = ext_hooks.after_tool_call.clone().unwrap_or_else(|| {
            std::sync::Arc::new(|_id, _name, _result| None)
        });
        if chained_before.is_some() || ext_hooks.after_tool_call.is_some() {
            let b = chained_before.unwrap_or_else(|| {
                std::sync::Arc::new(|_id, _name, _args| None)
            });
            *policy_slot.lock().unwrap() = Some((b, after_for_sub));
            policy_hooks = policy_slot.lock().unwrap().clone();
        }
        // Extension before/after hooks always install (weak-model loop detection,
        // verify-after-edit tracking). YOLO skips interactive approval prompts
        // but still chains `--agent-profile` hard denials when profile != default.
        hooks.before_tool_call = chain_gate_with_extensions(
            gate_hook.clone(),
            ext_hooks.before_tool_call,
            yolo,
            safety,
        );
        {
            let rhai_after = ext_hooks.after_tool_call;
            let graph_after = graph.clone().map(|g| {
                let g = std::sync::Arc::clone(&g);
                let g = std::sync::Arc::clone(&g);
                let cwd2 = cwd.clone();
                let f: pirs_agent::events::AfterToolCallHook =
                    std::sync::Arc::new(move |_id, name, result| {
                        if (name != "edit"
                            && name != "edit_block"
                            && name != "write"
                            && name != "ast_edit")
                            || result.is_error
                        {
                            return None;
                        }
                        let path = result
                            .details
                            .as_ref()
                            .and_then(|d| d.get("path"))
                            .and_then(|p| p.as_str())
                            .map(std::path::PathBuf::from)
                            .or_else(|| {
                                result
                                    .content
                                    .iter()
                                    .filter_map(|b| b.as_text())
                                    .next()
                                    .and_then(|t| {
                                        t.rsplit_once(" in ")
                                            .map(|(_, p)| std::path::PathBuf::from(p.trim()))
                                    })
                            })?;
                        let abs = if path.is_absolute() {
                            path
                        } else {
                            cwd2.join(path)
                        };
                        let graph = g.get();
                        let mut notes: Vec<String> = Vec::new();
                        for sym in graph.file_symbols(&abs) {
                            let n = graph.callers(&sym.name).len();
                            if n > 0 {
                                notes.push(format!(
                                    "{} ({} caller{})",
                                    sym.name,
                                    n,
                                    if n == 1 { "" } else { "s" }
                                ));
                            }
                        }
                        // Mark the index stale on EVERY edit — before any early
                        // return — so the next graph query rebuilds against the
                        // edited tree. (Previously this ran only when notes were
                        // non-empty and was skipped entirely when a rhai after-
                        // hook returned Some, freezing the graph.)
                        g.invalidate();
                        if notes.is_empty() {
                            return None;
                        }
                        let mut content = result.content.clone();
                        let total_callers: usize = graph
                            .file_symbols(&abs)
                            .iter()
                            .map(|s| graph.callers(&s.name).len())
                            .sum();
                        content.push(pirs_ai::ContentBlock::text(format!(
                            "Blast radius: {} graph caller(s) of edited symbols: {}",
                            total_callers,
                            notes.join(", ")
                        )));
                        Some(pirs_agent::ToolResultPatch {
                            content: Some(content),
                            ..Default::default()
                        })
                    });
                f
            });
            hooks.after_tool_call = match (rhai_after, graph_after) {
                // Always run the graph hook (for its invalidation side-effect),
                // then prefer the rhai patch, falling back to the blast-radius
                // note. Running graph_after unconditionally is what keeps the
                // index fresh when an extension's after-hook returns Some.
                (Some(r), Some(g)) => Some(std::sync::Arc::new(move |id, name, result| {
                    let graph_patch = g(id, name, result);
                    r(id, name, result).or(graph_patch)
                })),
                (a, b) => a.or(b),
            };
        }
        hooks.transform_context = ext_hooks.transform_context;
        hooks.should_stop_after_turn = ext_hooks.should_stop_after_turn;
        hooks.get_steering_messages = ext_hooks.get_steering_messages;
        hooks.get_follow_up_messages = ext_hooks.get_follow_up_messages;
        Some(h)
    };

    // The approval gate must be installed even with --no-extensions: the
    // chained install above only runs in the extensions branch, and without
    // this fallback `--approval ask --no-extensions` had no gate at all.
    install_gate_if_absent(&mut hooks, &gate_hook, &cli.approval);
    // yolo + --agent-profile plan (etc.) with no extensions: still enforce denials.
    install_profile_under_yolo_if_needed(&mut hooks, &gate_hook, &cli.approval, safety);

    // Subagents must inherit profile/approval even when --no-extensions left
    // policy_slot empty (previously only filled inside the extensions branch).
    {
        let yolo =
            crate::approval::ApprovalMode::parse(&cli.approval) == Some(crate::approval::ApprovalMode::Yolo);
        if policy_slot.lock().unwrap().is_none() {
            if let Some(b) =
                chain_gate_with_extensions(gate_hook.clone(), None, yolo, safety)
            {
                *policy_slot.lock().unwrap() = Some((
                    b,
                    std::sync::Arc::new(|_id, _name, _result| None),
                ));
            }
        }
    }

    let mut has_mcp = false;
    if !cli.no_mcp {
        let mcp = pirs_mcp::load_servers(&cwd).await;
        for err in &mcp.errors {
            eprintln!("[mcp error] {err}");
        }
        match mcp.mode {
            pirs_mcp::McpLoadMode::Eager if !mcp.handles.is_empty() => {
                let names: Vec<String> = mcp.handles.iter().map(|h| h.name.clone()).collect();
                eprintln!(
                    "[mcp eager: {} ({} tools)]",
                    names.join(", "),
                    mcp.tools.len()
                );
                has_mcp = true;
            }
            pirs_mcp::McpLoadMode::CatalogRouter => {
                eprintln!(
                    "[mcp catalog-router: catalog={} agent_tools={} max_live={} — use mcp_search/mcp_call]",
                    mcp.catalog_size,
                    mcp.tools.len(),
                    mcp.pool.as_ref().map(|p| p.max_live()).unwrap_or(0)
                );
                has_mcp = true;
            }
            _ => {}
        }
        let mut rep = pirs_mcp::McpDegradedReport::from_load(&mcp);
        if let Some(pool) = &mcp.pool {
            let st = pool.status().await;
            rep.live_count = st.live.len();
            rep.max_live = st.max_live;
            rep.catalog_size = st.catalog_size;
        }
        if rep.catalog_size > 0 || !rep.working.is_empty() || !rep.failed.is_empty() {
            std::env::set_var("PIRS_MCP_DOCTOR_LINES", rep.lines().join("\n"));
        }
        if !mcp.tools.is_empty() {
            has_mcp = true;
        }
        tools.extend(mcp.tools);
    }

    let skills = crate::discovery::discover_skills(&cwd);
    let file_commands = crate::discovery::discover_commands(&cwd);
    // Shared skill tools (same crate as pirs-claw).
    tools.extend(pirs_skills::skill_tools(
        std::sync::Arc::new(skills.clone()),
        true,
    ));
    // Runtime self-inspection tool (LLM + /status).
    tools.push(Arc::new(crate::runtime_features::SessionStateTool::new()));

    // Inject a PageRank-ranked symbol sketch so the model sees structure
    // without a first tool call (classic repomap idea). Weak mode gets a
    // larger budget (weaker models thrash without a map).
    let repo_map = if cli.no_repo_map {
        None
    } else {
        graph.as_ref().and_then(|g| {
            let budget = if cli.weak {
                6_000
            } else {
                pirs_graph::repo_map::DEFAULT_MAP_CHARS
            };
            pirs_graph::repo_map::render_sketch(&g.get(), &cwd, budget)
        })
    };
    if let Some(ref m) = repo_map {
        if crate::system_prompt::map_inject_is_material(Some(m.as_str())) {
            eprintln!("[repo_map: {} chars]", m.len());
        }
    }

    // Publish inspectable runtime snapshot *before* system prompt so the LLM
    // section includes autonomy, packs, strategy, and non-tool capabilities.
    let pack_names: Vec<String> = host
        .as_ref()
        .map(|h| h.extension_names())
        .unwrap_or_default();
    let slash_cmds: Vec<(String, String)> = {
        let mut v: Vec<(String, String)> = file_commands
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect();
        if let Some(h) = &host {
            v.extend(h.commands());
        }
        v
    };
    let has_lsp = ["rust-analyzer", "typescript-language-server", "pyright-langserver", "gopls"]
        .iter()
        .any(|b| {
            std::process::Command::new("sh")
                .args(["-c", &format!("command -v {b} >/dev/null 2>&1")])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
    let ui_mode = if cli.mode == "tui" {
        "tui"
    } else if cli.prompt.is_empty() {
        "repl"
    } else {
        "one-shot"
    };
    let rt = crate::runtime_features::collect(
        &cwd,
        ui_mode,
        &cli.model,
        cli.plan_model.as_deref(),
        cli.strategy.as_deref(),
        cli.profile.as_deref(),
        &cli.approval,
        cli.weak,
        &tools,
        &pack_names,
        &slash_cmds,
        graph.is_some(),
        has_mcp,
        has_lsp,
    );
    crate::runtime_features::publish(rt);
    eprintln!(
        "[runtime: autonomy={} tools={} packs={} — session_state tool + /status]",
        pirs_tools::live_permission_mode().name(),
        tools.len(),
        pack_names.len()
    );

    // Memory before prefix so auto-recall can inject without a `recall` tool call.
    if let Err(e) = pirs_agent::memory::init_global(&cwd.join(".pirs").join("memory.db")) {
        eprintln!("[memory disabled: {e}]");
    }
    let prompt_query = cli.prompt.join(" ");
    let auto_recall = pirs_agent::memory::global().and_then(|store| {
        let section = store.auto_recall_section(&prompt_query, &[], 5);
        if section.trim().is_empty() {
            None
        } else {
            Some(section)
        }
    });
    if let Some(ref r) = auto_recall {
        eprintln!("[auto_recall: {} chars]", r.len());
    }

    let mut system = crate::system_prompt::build_system_prompt_full(
        &cwd,
        &tools,
        repo_map.as_deref(),
        cli.weak,
        auto_recall.as_deref(),
    );
    // Progressive agentskills index (shared with pirs-claw) via discovery helper.
    if let Some(block) = crate::discovery::skills_prompt_block(&skills) {
        system.push_str(&block);
    }
    if let Some(h) = &host {
        let cmds = h.commands();
        if !cmds.is_empty() {
            system.push_str("\nCustom commands (from extensions):\n");
            for (name, desc) in &cmds {
                system.push_str(&format!("- /{name}: {desc}\n"));
            }
        }
    }
    if let Some(ctx) = crate::system_prompt::read_project_context(&cwd) {
        system.push_str(&ctx);
    }
    // Interactive role: stamp profile persona onto the system prompt so TUI/REPL
    // honor `--profile` beyond pack selection. Strategy seed is applied below
    // when launching the TUI.
    if let Some(p) = cli.profile.as_deref() {
        if let Ok(prof) = pirs_rhai::discover::resolve_profile(p, &cwd) {
            if let Some(persona) = prof.persona.as_deref() {
                if !persona.is_empty() {
                    system = format!("{persona}\n\n{system}");
                    eprintln!("[profile persona: {}]", prof.name);
                }
            }
        }
    }

    let completion = CompletionOptions {
        api_key: Some(api_key.clone()),
        ..Default::default()
    };

    let compaction = if cli.no_compaction {
        None
    } else {
        Some(pirs_agent::compaction::CompactionConfig {
            context_window: cli.context_window,
            ..Default::default()
        })
    };

    {
        let delegate_provider: std::sync::Arc<dyn pirs_ai::LlmProvider> = if cli.provider
            == "anthropic"
        {
            std::sync::Arc::new(
                pirs_ai::AnthropicClient::new(cli.base_url.clone())
                    .with_max_retries(cli.max_retries),
            )
        } else {
            std::sync::Arc::new(
                pirs_ai::OpenAiCompat::new(cli.base_url.clone()).with_max_retries(cli.max_retries),
            )
        };
        let delegate_completion = CompletionOptions {
            api_key: Some(api_key.clone()),
            ..Default::default()
        };
        let delegate_model = cli.model.clone();
        let _delegate_cwd = cwd.clone();
        let delegate_tools = sub_tools.clone();
        let delegate = pirs_agent::delegate::DelegateTool::new(
            delegate_provider,
            delegate_model,
            delegate_completion,
            move || delegate_tools.clone(),
        );
        if let Some((b, a)) = &policy_hooks {
            delegate.with_policy_hooks(b.clone(), a.clone());
        }
        tools.push(delegate);
    }

    let (visible, mut tools) = if cli.tool_diet {
        let set: pirs_agent::agent_loop::VisibleTools = std::sync::Arc::new(std::sync::Mutex::new(
            pirs_agent::use_tool::UseTool::default_visible(),
        ));
        let use_tool = pirs_agent::use_tool::UseTool::new(&set, &tools);
        tools.push(use_tool);
        (Some(set), tools)
    } else {
        (None, tools)
    };
    let _ = &mut tools;

    let execution = if cli.sequential {
        pirs_agent::ExecutionMode::Sequential
    } else {
        pirs_agent::ExecutionMode::Parallel
    };

    // approval_mode already resolved with autonomy above.
    if approval_mode == crate::approval::ApprovalMode::Yolo {
        eprintln!(
            "[WARNING: autonomy full / yolo — no approval prompts; tools follow autonomy ladder]"
        );
    }

    let cascade_cfg =
        cli.cascade
            .as_ref()
            .map(|draft_model| pirs_agent::agent_loop::CascadeConfig {
                draft_model: draft_model.clone(),
                judge: crate::subagent::build_cascade_judge(
                    std::sync::Arc::clone(&provider),
                    draft_model.clone(),
                ),
            });

    // Strategy/profile mode needs the full tool list after the agent takes it, to
    // re-scope tools per phase. Clone the Arc handles up front (cheap) only then.
    let strategy_mode = cli.strategy.is_some() || cli.profile.is_some();
    // Resolve hybrid report pins once — every one-shot / REPL exit must use these
    // so print sites cannot hardcode empty plan-model / strategy.
    let report_pins = crate::session_stats::ReportPins::from_cli(
        cli.plan_model.clone(),
        cli.strategy.clone(),
        cli.profile.clone(),
    );
    // TUI can enable strategy mid-session, so keep a full tool clone whenever
    // we might run phases (strategy/profile mode or TUI).
    let strategy_tools: Vec<Arc<dyn AgentTool>> =
        if strategy_mode || cli.mode == "tui" {
            tools.clone()
        } else {
            Vec::new()
        };

    let mut agent = Agent::new(provider, &cli.model)
        .with_system_prompt(system)
        .with_tools(tools)
        .with_completion(completion)
        .with_hooks(hooks)
        .with_compaction(compaction)
        .with_visible_tools(visible)
        .with_tool_execution(execution)
        .with_cascade(cascade_cfg)
        .with_budgets(pirs_agent::agent_loop::Budgets {
            max_turns: cli.max_turns,
            max_tool_calls: cli.max_tool_calls,
            max_wall_time: cli.max_wall_time.map(std::time::Duration::from_secs),
        });
    agent.set_extra_usage_handle(usage_slot.clone());
    {
        let steer = agent.steer_sender();
        pirs_agent::jobs::registry().set_notifier(std::sync::Arc::new(move |msg| {
            steer(Message::user(msg));
        }));
    }
    let approval_shared = gate.shared_mode();

    let session_path = crate::session::session_path(&cwd)?;
    let session_stem = session_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    pirs_rhai::set_session_meta(&session_stem, &cli.model);

    // First-class action audit (not pack-only). Disable with PIRS_AUDIT=0.
    {
        let audit = pirs_agent::AuditLog::default_open();
        if pirs_agent::audit_enabled() {
            eprintln!("[audit: {}]", audit.path().display());
        }
        agent.subscribe(pirs_agent::audit_listener(audit));
    }

    // Optional flight recorder: agent events + strategy phase boundaries.
    let run_id = crate::observability::make_run_id(&session_stem);
    let trace_path = crate::observability::resolve_trace_path(cli.trace.as_deref(), &run_id);
    let trace_phase: Arc<Mutex<String>> = Arc::new(Mutex::new("main".into()));
    let recorder: Option<Arc<pirs_agent::trace::Recorder>> = match &trace_path {
        Some(path) => {
            let rec = crate::observability::open_recorder(path, &run_id)?;
            let aliases: Vec<String> = model_registry
                .models
                .iter()
                .map(|m| m.alias.clone())
                .collect();
            crate::observability::record_run_config(
                &rec,
                &cli.model,
                cli.plan_model.as_deref(),
                cli.strategy.as_deref().or(cli.profile.as_deref()),
                &aliases,
            );
            crate::observability::attach_agent_trace(
                &mut agent,
                Arc::clone(&rec),
                Arc::clone(&trace_phase),
            );
            Some(rec)
        }
        None => None,
    };
    // Memory may already be open for auto-recall inject; re-scope to this session
    // so tool-time `recall` stays session-local (prefix used cross-session pool).
    if pirs_agent::memory::global().is_none() {
        if let Err(e) = pirs_agent::memory::init_global(&cwd.join(".pirs").join("memory.db")) {
            eprintln!("[memory disabled: {e}]");
        }
    }
    if pirs_agent::memory::global().is_some() {
        pirs_agent::memory::set_session(
            &session_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        );
    }
    if cli.resume {
        match crate::session::load_latest(&cwd) {
            Ok((path, messages)) => {
                eprintln!("[resumed {} ({} messages)]", path.display(), messages.len());
                let n = messages.len();
                agent.messages = messages;
                crate::session::append(&session_path, &agent.messages)?;
                eprintln!("[carried {} messages into the new session file]", n);
            }
            Err(e) => eprintln!("[no session to resume: {e}]"),
        }
    }

    let printer = Arc::new(Printer::new());
    let session_file_shared = std::sync::Arc::new(std::sync::Mutex::new(session_path.clone()));
    {
        let sf = std::sync::Arc::clone(&session_file_shared);
        agent.subscribe(Arc::new(move |event: AgentEvent| {
            if let AgentEvent::MessageEnd { message } = event {
                let path = sf.lock().unwrap().clone();
                let _ = crate::session::append(&path, &[*message]);
            }
        }));
    }
    if let Some(h) = &host {
        if let Some(l) = h.listener() {
            agent.subscribe(l);
        }
    }
    let printed = Arc::new(Mutex::new((0usize, 0usize)));
    if cli.mode == "repl" {
        let p = Arc::clone(&printer);
        let printed = Arc::clone(&printed);
        agent.subscribe(Arc::new(move |event: AgentEvent| match &event {
            AgentEvent::MessageStart { message } if message.is_assistant() => {
                *printed.lock().unwrap() = (0, 0);
            }
            AgentEvent::MessageUpdate { message } => {
                let text = message.text();
                let thinking: String = message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        pirs_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                let mut n = printed.lock().unwrap();
                if thinking.len() > n.1 {
                    print!("\x1b[2;3m{}\x1b[0m", &thinking[n.1..]);
                    let _ = std::io::stdout().flush();
                    n.1 = thinking.len();
                }
                if text.len() > n.0 {
                    print!("{}", &text[n.0..]);
                    let _ = std::io::stdout().flush();
                    n.0 = text.len();
                }
            }
            AgentEvent::MessageEnd { message } if message.is_assistant() => {
                // The streamed assistant text/thinking above is printed as raw
                // deltas with no trailing newline. Terminate the line here so
                // the next `pirs> ` prompt starts at column 0. Without this the
                // prompt renders glued to the end of the response, and rustyline
                // -- which assumes it starts at column 0 -- miscomputes cursor
                // columns on every refresh and visibly drops/scrambles the
                // characters you type ("cool, you have a handoff command" ->
                // "oy a hdfoa"). Printer::MessageEnd can't do this: its guard
                // (`if *streaming`) never trips because this callback consumes
                // the assistant MessageStart before Printer ever sees it. Only
                // emit the newline when we actually streamed something, then let
                // Printer still surface a terminal-error stop_reason.
                {
                    let mut n = printed.lock().unwrap();
                    if n.0 > 0 || n.1 > 0 {
                        println!();
                        *n = (0, 0);
                    }
                }
                let _ = std::io::stdout().flush();
                p.event(event);
            }
            _ => p.event(event),
        }));
    }

    // Browser UI: `--mode web` or legacy `--serve`. Same full agent as TUI/REPL
    // (tools, profile packs, MCP, graph, …) — not the thin rpc/acp bootstrap.
    if cli.serve || cli.mode == "web" {
        let loopback = matches!(cli.bind.as_str(), "127.0.0.1" | "localhost" | "::1");
        let token = match cli.serve_token.clone() {
            Some(t) if t.trim().is_empty() => {
                // Empty token makes constant_time_eq(b"", b"") succeed → open auth (M-35).
                anyhow::bail!(
                    "--serve-token must be non-empty (empty token disables auth). \
                     Omit the flag to auto-generate a token on loopback, or pass a real secret."
                );
            }
            Some(t) => t,
            None => {
                if !loopback {
                    anyhow::bail!(
                        "--serve-token (or PIRS_SERVE_TOKEN) is required for a non-loopback bind ({})",
                        cli.bind
                    );
                }
                generate_serve_token()
            }
        };
        if token.trim().is_empty() {
            anyhow::bail!("serve token resolved empty; refusing to start without auth");
        }
        eprintln!("[serve token: {token}]");
        return crate::serve::run(crate::serve::ServeOptions {
            agent,
            host,
            port: cli.port,
            bind: cli.bind.clone(),
            token,
            allow_external: cli.serve_external,
        })
        .await;
    }

    if cli.mode == "tui" {
        let aliases: Vec<String> = model_registry
            .models
            .iter()
            .map(|m| m.alias.clone())
            .collect();
        // Seed strategy from --strategy, else from --profile's strategy name.
        let tui_strategy = if let Some(s) = cli.strategy.clone() {
            Some(s)
        } else if let Some(p) = cli.profile.as_deref() {
            pirs_rhai::discover::resolve_profile(p, &cwd)
                .ok()
                .map(|prof| prof.strategy.name)
        } else {
            None
        };
        if let Some(ref s) = tui_strategy {
            eprintln!("[tui strategy: {s}]");
        }
        return tui::run(tui::TuiOptions {
            agent,
            host,
            session_path,
            approval_mode,
            approval_gate: Some(gate),
            cwd,
            strategy: tui_strategy,
            plan_model: cli.plan_model.clone(),
            verify: cli.verify.clone(),
            max_attempts: cli.max_attempts,
            strategy_tools,
            recorder: recorder.clone(),
            trace_phase: Some(Arc::clone(&trace_phase)),
            model_aliases: aliases,
        })
        .await;
    }

    if let Some(prompt) = cli.prompt.first().cloned() {
        if strategy_mode {
            let (report, passed) = run_strategy_turn(
                &agent,
                &prompt,
                cli.strategy.as_deref(),
                cli.profile.as_deref(),
                &cli.model,
                cli.plan_model.as_deref(),
                strategy_tools,
                &cwd,
                cli.verify.as_deref(),
                cli.max_attempts,
                recorder.as_ref(),
                Some(Arc::clone(&trace_phase)),
            )
            .await?;
            eprintln!();
            // Hybrid plan-exec path: role-split via shared format_usage_end + ReportPins.
            crate::session_stats::print_usage_end(&report, &cli.model, &report_pins);
            // A --verify gate (including weak auto-verify) that never passed
            // exits non-zero so scripts/CI can tell a green run from a red one.
            if cli.verify.is_some() && !passed {
                std::process::exit(1);
            }
            return Ok(());
        }
        run_turn(
            &mut agent,
            &prompt,
            &printer,
            &session_path,
            approval_mode,
            host.as_ref(),
            true, // one-shot: no rustyline readline follows, safe to steer from stdin
        )
        .await?;
        // Shared learning loop (same as pirs-claw): crystallize after substantial one-shots.
        if pirs_skills::learn_enabled_cli() {
            let reply = agent
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    pirs_ai::Message::Assistant(a) => {
                        let t = a.text();
                        if t.trim().is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let transcript =
                pirs_skills::session_transcript(&prompt, &reply, "pirs one-shot");
            let _ = pirs_skills::maybe_crystallize_skill(
                agent.provider.clone(),
                &agent.model,
                Some(api_key.clone()),
                &transcript,
                400,
            )
            .await;
        }
        eprintln!();
        crate::session_stats::print_usage_end(&agent.usage_report(), &cli.model, &report_pins);
        if let Some(hit) = agent.budget_hit {
            eprintln!("[budget exhausted: {hit:?}]");
            std::process::exit(match hit {
                pirs_agent::agent_loop::BudgetHit::Turns => 53,
                pirs_agent::agent_loop::BudgetHit::WallTime => 54,
                pirs_agent::agent_loop::BudgetHit::ToolCalls => 55,
            });
        }
        return Ok(());
    }

    repl(
        &mut agent,
        &printer,
        &session_file_shared,
        &cwd,
        host.as_ref(),
        &file_commands,
        approval_shared,
        &report_pins,
    )
    .await
}


fn generate_serve_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("getrandom failed to produce a serve token");
    let mut s = String::with_capacity(64);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
