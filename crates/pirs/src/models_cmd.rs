//! `pirs backends` / `pirs models` — list, refresh, search model catalogs.

use std::path::Path;

use anyhow::{bail, Context};

use crate::registry;

/// Handle pseudo-subcommands that do not need a full agent session.
/// Returns `true` if the command was handled (caller should exit).
pub fn try_run(cwd: &Path, prompt: &str) -> anyhow::Result<bool> {
    let parts: Vec<&str> = prompt.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(false);
    }
    match parts[0] {
        "backends" => {
            cmd_backends(cwd)?;
            Ok(true)
        }
        "models" => {
            cmd_models(cwd, &parts[1..])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn load_reg(cwd: &Path) -> pirs_ai::RegistryFile {
    registry::load_secrets_env();
    registry::load_registry_layers(cwd)
}

fn cmd_backends(cwd: &Path) -> anyhow::Result<()> {
    let reg = load_reg(cwd);
    println!("backends (builtin + ~/.pirs/config.toml):");
    println!(
        "  {:<18} {:<12} {:<28} {}",
        "NAME", "KEY", "ENV", "BASE"
    );
    for b in &reg.backends {
        let env = b.api_key_env.as_deref().unwrap_or("-");
        let has = pirs_ai::backend_key_present(b);
        let key = if has { "yes" } else { "no" };
        let base = if b.base_url.len() > 40 {
            format!("{}…", &b.base_url[..37])
        } else {
            b.base_url.clone()
        };
        println!("  {:<18} {:<12} {:<28} {}", b.name, key, env, base);
        if let Some((n, age, stale)) = pirs_ai::catalog_status(&b.name) {
            let stale_s = if stale { " stale" } else { "" };
            println!("      catalog: {n} models, age {age}s{stale_s}");
        }
    }
    println!();
    println!("pin a subscription:  --model <name>/<remote-id>");
    println!("  e.g. openrouter/deepseek/deepseek-v4-flash");
    println!("  e.g. dashscope/qwen3.5-plus");
    println!("add another OpenRouter key: [[backends]] name=openrouter-work api_key_env=…");
    Ok(())
}

fn cmd_models(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let reg = load_reg(cwd);
    if args.is_empty() {
        print_models_summary(&reg);
        return Ok(());
    }
    match args[0] {
        "refresh" => {
            // reqwest::blocking cannot run on a tokio worker; isolate it.
            if args.len() >= 2 {
                let name = args[1].to_string();
                print!("refreshing {name}… ");
                let reg = reg.clone();
                let (cat, path) = tokio::task::block_in_place(|| {
                    pirs_ai::refresh_backend(&reg, &name)
                })?;
                println!(
                    "ok — {} models → {}",
                    cat.models.len(),
                    path.display()
                );
            } else {
                println!("refreshing active backends (keys present)…");
                let reg = reg.clone();
                let results = tokio::task::block_in_place(|| pirs_ai::refresh_active(&reg));
                if results.is_empty() {
                    println!("  (no backends with keys — set OPENROUTER_API_KEY / DASHSCOPE_API_KEY / …)");
                }
                for (name, res) in results {
                    match res {
                        Ok(c) => println!("  {name}: {} models", c.models.len()),
                        Err(e) => println!("  {name}: error: {e}"),
                    }
                }
            }
            Ok(())
        }
        "search" => {
            let q = args.get(1).copied().unwrap_or("");
            if q.is_empty() {
                bail!("usage: pirs models search <query>");
            }
            let hits = pirs_ai::search_catalogs(&reg, q);
            if hits.is_empty() {
                println!(
                    "no cached hits for {q:?} — run `pirs models refresh` first, then search again"
                );
                return Ok(());
            }
            println!("catalog hits for {q:?}:");
            for (backend, m) in hits.iter().take(80) {
                let pin = pirs_ai::format_pin(backend, &m.id);
                if let Some(name) = &m.name {
                    println!("  {pin}  ({name})");
                } else {
                    println!("  {pin}");
                }
            }
            if hits.len() > 80 {
                println!("  … +{} more", hits.len() - 80);
            }
            println!("\npin with: --model <backend>/<id>");
            Ok(())
        }
        "list" => {
            // pirs models list [backend]
            let backend = args.get(1).copied();
            if let Some(name) = backend {
                let cat = pirs_ai::load_catalog(name).with_context(|| {
                    format!("no catalog for {name:?}; run: pirs models refresh {name}")
                })?;
                println!(
                    "{name}: {} models (age {}s{})",
                    cat.models.len(),
                    cat.age_secs(),
                    if cat.is_stale() { ", stale" } else { "" }
                );
                let prefix = args.get(2).map(|s| s.to_ascii_lowercase());
                for m in &cat.models {
                    if let Some(ref p) = prefix {
                        if !m.id.to_ascii_lowercase().starts_with(p)
                            && !m.id.to_ascii_lowercase().contains(p.as_str())
                        {
                            continue;
                        }
                    }
                    println!("  {}/{}", name, m.id);
                }
            } else {
                print_models_summary(&reg);
            }
            Ok(())
        }
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => {
            bail!(
                "unknown models subcommand {other:?}\n{}",
                help_text()
            );
        }
    }
}

fn print_models_summary(reg: &pirs_ai::RegistryFile) {
    println!("portable models (bare name → backends with keys):");
    let active: std::collections::HashSet<&str> = pirs_ai::active_backends(reg)
        .into_iter()
        .map(|b| b.name.as_str())
        .collect();
    for m in &reg.models {
        let targets: Vec<String> = m
            .serve
            .iter()
            .map(|s| {
                let mark = if active.contains(s.backend.as_str()) {
                    "*"
                } else {
                    " "
                };
                format!("{mark}{}/{}", s.backend, s.model)
            })
            .collect();
        let any = m.serve.iter().any(|s| active.contains(s.backend.as_str()));
        if !any {
            continue;
        }
        let tier = m.tier.as_deref().unwrap_or("-");
        println!(
            "  {:<22} [{tier}]  {}",
            m.alias,
            targets.join("  ")
        );
    }
    println!("  (* = key present)");
    println!();
    println!("pin mode:  --model backend/remote-id");
    println!("  examples:");
    println!("    dashscope/qwen3.5-plus");
    println!("    openrouter/deepseek/deepseek-v4-flash");
    println!("    openrouter-work/anthropic/claude-sonnet-4   # second OpenRouter account");
    println!();
    println!("catalogs:");
    for b in pirs_ai::active_backends(reg) {
        match pirs_ai::catalog_status(&b.name) {
            Some((n, age, stale)) => {
                println!(
                    "  {:<18} {n} models, age {age}s{}",
                    b.name,
                    if stale { " (stale)" } else { "" }
                );
            }
            None => println!("  {:<18} (not cached — pirs models refresh {})", b.name, b.name),
        }
    }
    println!();
    println!("{}", help_text());
}

fn help_text() -> &'static str {
    "usage:\n  \
     pirs backends\n  \
     pirs models\n  \
     pirs models refresh [backend]\n  \
     pirs models search <query>\n  \
     pirs models list <backend> [filter]"
}

fn print_help() {
    println!("{}", help_text());
}
