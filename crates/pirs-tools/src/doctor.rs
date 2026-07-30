//! Runtime doctor / status for harness + shared diagnostics.

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use serde_json::Value;

/// Env key names probed for presence (values never printed).
pub const DOCTOR_KEY_ENVS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "DEEPSEEK_API_KEY",
    "GROQ_API_KEY",
    "TELEGRAM_BOT_TOKEN",
    "PIRS_TELEGRAM_BOT_TOKEN",
];

/// Pure: which of `DOCTOR_KEY_ENVS` are set and non-empty (names only).
pub fn doctor_api_keys_set() -> Vec<&'static str> {
    let mut keys = Vec::new();
    for &k in DOCTOR_KEY_ENVS {
        if std::env::var(k)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            keys.push(k);
        }
    }
    keys
}

/// Pure: MCP config presence lines for project + user paths (no secret contents).
pub fn doctor_mcp_config_lines(cwd: &Path, home: Option<&Path>) -> Vec<String> {
    let mut lines = Vec::new();
    let mcp = cwd.join(".mcp.json");
    lines.push(format!(
        "mcp_config: {}",
        if mcp.is_file() {
            format!("present ({})", mcp.display())
        } else {
            "absent (cwd .mcp.json)".into()
        }
    ));
    if let Some(h) = home {
        let um = h.join(".pirs").join("mcp.json");
        lines.push(format!(
            "mcp_user_config: {}",
            if um.is_file() {
                format!("present ({})", um.display())
            } else {
                "absent (~/.pirs/mcp.json)".into()
            }
        ));
    }
    lines.push(
        "mcp_email_calendar: MCP connectors only (no first-party OAuth); \
         see docs/mcp-email-calendar.md"
            .into(),
    );
    lines
}

/// Pure: browser/CDP readiness lines (PATH probe + env URL set/absent).
pub fn doctor_browser_lines(
    which_bin: impl Fn(&str) -> bool,
    cdp_url: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut chrome = false;
    for n in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ] {
        if which_bin(n) {
            chrome = true;
            lines.push(format!("browser: {n} on PATH"));
            break;
        }
    }
    if !chrome && Path::new("/snap/bin/chromium").is_file() {
        chrome = true;
        lines.push("browser: /snap/bin/chromium present".into());
    }
    if !chrome {
        lines.push("browser: no chromium/chrome on PATH (CDP auto-launch unavailable)".into());
    }
    match cdp_url {
        Some(u) if !u.trim().is_empty() => lines.push("browser_cdp_url: set".into()),
        _ => lines.push("browser_cdp_url: unset (auto-launch or default :9222)".into()),
    }
    lines
}

/// Pure: messaging channel honesty (Telegram spine vs stubs).
pub fn doctor_channel_policy_lines(telegram_token_set: bool) -> Vec<String> {
    vec![
        format!(
            "channel_telegram: {} (spine)",
            if telegram_token_set {
                "token set"
            } else {
                "token missing"
            }
        ),
        "channel_discord: stub/thin (not production depth)".into(),
        "channel_slack: stub/thin (not production depth)".into(),
        "channel_whatsapp: stub/thin (not production depth)".into(),
        "channel_signal: stub/thin (not production depth)".into(),
        "schedule_fires: require an LLM API key for chat body".into(),
        "pairing: allowlist peer ids + optional short pairing codes (mint/redeem)".into(),
    ]
}

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
    let keys = doctor_api_keys_set();
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
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    lines.extend(doctor_mcp_config_lines(cwd, home.as_deref()));
    lines.push(
        "mcp_scale: catalog+lazy pool when servers > PIRS_MCP_EAGER_MAX (default 8); \
         router tools mcp_search/mcp_call; live cap PIRS_MCP_MAX_LIVE (default 16); \
         see docs/mcp-scale.md"
            .into(),
    );
    if let Ok(report) = std::env::var("PIRS_MCP_DOCTOR_LINES") {
        for line in report.lines().filter(|l| !l.is_empty()) {
            lines.push(format!("  {line}"));
        }
    }

    // Git
    let git = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    lines.push(match git {
        Ok(o) if o.status.success() => "git: ok".into(),
        _ => "git: not a repo or git missing".into(),
    });

    // Browser / CDP
    let cdp = std::env::var("PIRS_BROWSER_CDP_URL").ok();
    lines.extend(doctor_browser_lines(which, cdp.as_deref()));

    // Channel policy honesty
    let tg = keys
        .iter()
        .any(|k| *k == "TELEGRAM_BOT_TOKEN" || *k == "PIRS_TELEGRAM_BOT_TOKEN");
    lines.extend(doctor_channel_policy_lines(tg));

    // Computer use
    let cu = matches!(
        std::env::var("PIRS_COMPUTER_USE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    );
    let display = std::env::var("DISPLAY").unwrap_or_default();
    let display_ok = !display.trim().is_empty() && display_reachable(&display);
    lines.push(format!(
        "computer_use: {} (scrot={} xdotool={} DISPLAY={}{})",
        if cu {
            "enabled"
        } else {
            "off (set PIRS_COMPUTER_USE=1)"
        },
        which("scrot"),
        which("xdotool"),
        if display.is_empty() {
            "(unset)"
        } else {
            display.as_str()
        },
        if !cu {
            ""
        } else if display_ok {
            " reachable"
        } else if display.is_empty() {
            " — no DISPLAY; use a desktop session or: xvfb-run -a pirs …"
        } else {
            " — not reachable (start X / xvfb-run -a pirs …)"
        }
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
    let home_s = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let soul = PathBuf::from(&home_s).join(".pirs").join("soul.md");
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

/// True if `$DISPLAY` socket looks usable (does not require xdotool).
fn display_reachable(display: &str) -> bool {
    // :0 → /tmp/.X11-unix/X0 ; :99 → X99
    let num = display
        .trim()
        .trim_start_matches(':')
        .split(['.', '-'])
        .next()
        .unwrap_or("");
    if num.is_empty() {
        return false;
    }
    let sock = std::path::PathBuf::from(format!("/tmp/.X11-unix/X{num}"));
    sock.exists()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize env-mutating doctor tests (process-global env).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn doctor_api_keys_reports_names_not_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::set_var("OPENAI_API_KEY", "sk-secret-must-not-appear");
        let keys = doctor_api_keys_set();
        assert!(keys.contains(&"OPENAI_API_KEY"));
        // Full report while key is set — names only, never the value.
        let joined = doctor_report(Path::new(".")).join("\n");
        match prev {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        assert!(joined.contains("api_keys_set:"));
        assert!(joined.contains("OPENAI_API_KEY"));
        assert!(
            !joined.contains("sk-secret-must-not-appear"),
            "doctor must never print secret values: {joined}"
        );
    }

    #[test]
    fn doctor_mcp_config_absent_vs_present() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".pirs")).unwrap();
        let absent = doctor_mcp_config_lines(dir.path(), Some(&home));
        assert!(absent.iter().any(|l| l.contains("mcp_config: absent")));
        assert!(absent.iter().any(|l| l.contains("mcp_user_config: absent")));
        std::fs::write(dir.path().join(".mcp.json"), "{}").unwrap();
        std::fs::write(home.join(".pirs").join("mcp.json"), "{}").unwrap();
        let present = doctor_mcp_config_lines(dir.path(), Some(&home));
        assert!(present.iter().any(|l| l.contains("mcp_config: present")));
        assert!(present
            .iter()
            .any(|l| l.contains("mcp_user_config: present")));
        assert!(present.iter().any(|l| l.contains("MCP connectors only")));
    }

    #[test]
    fn doctor_browser_reports_cdp_url_set_or_unset() {
        let no_chrome = |_b: &str| false;
        let unset = doctor_browser_lines(no_chrome, None);
        assert!(unset.iter().any(|l| l.contains("browser_cdp_url: unset")));
        // PATH miss may still find snap binary on this host.
        assert!(
            unset
                .iter()
                .any(|l| l.contains("no chromium") || l.contains("/snap/bin/chromium")),
            "{unset:?}"
        );
        let set = doctor_browser_lines(|b| b == "chromium", Some("http://127.0.0.1:9222"));
        assert!(set.iter().any(|l| l.contains("browser: chromium on PATH")));
        assert!(set.iter().any(|l| l.contains("browser_cdp_url: set")));
        // Presence flag only — never echo the URL (may carry tokens in real deploys).
        assert!(!set.join("\n").contains("http://"));
    }

    #[test]
    fn doctor_channel_policy_labels_stubs_and_telegram() {
        let with = doctor_channel_policy_lines(true);
        let without = doctor_channel_policy_lines(false);
        assert!(with
            .iter()
            .any(|l| l.contains("channel_telegram: token set")));
        assert!(without.iter().any(|l| l.contains("token missing")));
        assert!(with.iter().any(|l| l.contains("channel_discord: stub")));
        assert!(with
            .iter()
            .any(|l| l.contains("schedule_fires: require an LLM")));
        assert!(with.iter().any(|l| l.contains("allowlist peer ids")));
    }

    #[test]
    fn doctor_report_includes_honesty_lines() {
        let lines = doctor_report(Path::new("."));
        let j = lines.join("\n");
        assert!(j.contains("mcp_config:"));
        assert!(j.contains("channel_telegram:"));
        assert!(j.contains("channel_discord: stub"));
        assert!(j.contains("browser_cdp_url:"));
    }
}
