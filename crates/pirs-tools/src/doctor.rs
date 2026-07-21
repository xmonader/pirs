//! Runtime doctor / status for harness + shared diagnostics.

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use serde_json::Value;

/// Collect human-readable doctor lines (never prints secret values).
pub fn doctor_report(cwd: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("cwd: {}", cwd.display()));
    lines.push(format!(
        "audit: {} ({})",
        pirs_agent::default_audit_path().display(),
        if pirs_agent::audit_enabled() {
            "enabled"
        } else {
            "disabled (PIRS_AUDIT=0)"
        }
    ));

    // API keys present (names only)
    let mut keys = Vec::new();
    for k in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "DEEPSEEK_API_KEY",
        "GROQ_API_KEY",
        "TELEGRAM_BOT_TOKEN",
        "PIRS_TELEGRAM_BOT_TOKEN",
    ] {
        if std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false) {
            keys.push(k);
        }
    }
    lines.push(if keys.is_empty() {
        "api_keys: (none of common env keys set)".into()
    } else {
        format!("api_keys_set: {}", keys.join(", "))
    });

    let profile = std::env::var("PIRS_AGENT_PROFILE").unwrap_or_else(|_| "default".into());
    lines.push(format!("agent_profile: {profile}"));

    // Toolchain
    let prof = crate::project::detect_profile(cwd);
    lines.push(format!(
        "project_toolchain: {}",
        prof.toolchain.as_deref().unwrap_or("(none detected)")
    ));
    if let Some(t) = &prof.test {
        lines.push(format!("  test: {t}"));
    }

    // LSP servers on PATH
    let mut lsp = Vec::new();
    for (name, bin) in [
        ("rust", "rust-analyzer"),
        ("typescript", "typescript-language-server"),
        ("python", "pyright-langserver"),
        ("go", "gopls"),
    ] {
        if which(bin) {
            lsp.push(format!("{name}:{bin}"));
        }
    }
    lines.push(if lsp.is_empty() {
        "lsp_servers: (none of rust-analyzer/tsserver/pyright/gopls on PATH)".into()
    } else {
        format!("lsp_servers: {}", lsp.join(", "))
    });

    // MCP config
    let mcp = cwd.join(".mcp.json");
    lines.push(format!(
        "mcp_config: {}",
        if mcp.is_file() {
            format!("present ({})", mcp.display())
        } else {
            "absent".into()
        }
    ));

    // Git
    let git = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    lines.push(match git {
        Ok(o) if o.status.success() => "git: ok".into(),
        _ => "git: not a repo or git missing".into(),
    });

    // Chromium for CDP
    let mut chrome = false;
    for n in ["chromium", "google-chrome", "google-chrome-stable", "chrome"] {
        if which(n) {
            chrome = true;
            lines.push(format!("browser: {n} on PATH"));
            break;
        }
    }
    if !chrome {
        lines.push("browser: no chromium/chrome on PATH (CDP auto-launch unavailable)".into());
    }
    if let Ok(u) = std::env::var("PIRS_BROWSER_CDP_URL") {
        if !u.is_empty() {
            lines.push(format!("browser_cdp_url: set"));
        }
    }

    // Computer use
    let cu = matches!(
        std::env::var("PIRS_COMPUTER_USE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    lines.push(format!(
        "computer_use: {} (scrot={} xdotool={})",
        if cu { "enabled" } else { "off (PIRS_COMPUTER_USE=1)" },
        which("scrot"),
        which("xdotool")
    ));

    // gh
    lines.push(format!(
        "gh_cli: {}",
        if which("gh") {
            "on PATH"
        } else {
            "missing (pr create/checks limited)"
        }
    ));

    // Soul / memory
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let soul = PathBuf::from(&home).join(".pirs").join("soul.md");
    lines.push(format!(
        "soul: {}",
        if soul.is_file() {
            "present"
        } else {
            "missing (template on first use)"
        }
    ));

    lines
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p).any(|d| {
                let c = d.join(bin);
                c.is_file()
            })
        })
        .unwrap_or(false)
}

/// Agent tool: doctor
pub struct DoctorTool {
    cwd: PathBuf,
}

impl DoctorTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for DoctorTool {
    fn name(&self) -> &str {
        "doctor"
    }

    fn description(&self) -> &str {
        "Runtime diagnostics: API keys present (not values), toolchain, LSP servers, \
         MCP config, git, browser/CDP, computer-use, gh. Use when setup looks broken."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("doctor: environment/setup diagnostics")
    }

    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text(doctor_report(&self.cwd).join("\n")))
    }
}
