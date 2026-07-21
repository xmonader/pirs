//! Pure-Rust CDP browser automation via **chromiumoxide**.
//!
//! Connect to an existing Chrome/Chromium/Playwright CDP endpoint
//! (`PIRS_BROWSER_CDP_URL` / `BROWSER_CDP_URL`, default try `http://127.0.0.1:9222`)
//! or launch a local Chromium with remote debugging.
//!
//! Tools (single multi-action tool + helpers):
//! - `browser_cdp` — connect | goto | content | click | type | eval | screenshot | close | status
//!
//! Requires feature `cdp` (default on). Disable with `--no-default-features` on pirs-tools.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::web::html_to_text;

/// Shared CDP session for the process (one browser connection).
static SESSION: std::sync::OnceLock<Arc<Mutex<CdpSession>>> = std::sync::OnceLock::new();

fn session() -> Arc<Mutex<CdpSession>> {
    SESSION
        .get_or_init(|| Arc::new(Mutex::new(CdpSession::default())))
        .clone()
}

fn cdp_url_from_env() -> Option<String> {
    for k in ["PIRS_BROWSER_CDP_URL", "BROWSER_CDP_URL", "CDP_URL"] {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

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

struct CdpSession {
    browser: Option<chromiumoxide::Browser>,
    /// Keep handler task alive.
    _handler: Option<tokio::task::JoinHandle<()>>,
    page: Option<chromiumoxide::Page>,
    /// Child chromium we launched (kill on close / Drop).
    child: Option<Child>,
    endpoint: Option<String>,
    /// User-data dir for launched Chromium (removed on close).
    user_data_dir: Option<PathBuf>,
    last_error: Option<String>,
}

impl Default for CdpSession {
    fn default() -> Self {
        Self {
            browser: None,
            _handler: None,
            page: None,
            child: None,
            endpoint: None,
            user_data_dir: None,
            last_error: None,
        }
    }
}

impl Drop for CdpSession {
    fn drop(&mut self) {
        // Best-effort: kill launched Chromium so process exit doesn't leave zombies.
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(dir) = self.user_data_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl CdpSession {
    /// Probe whether the current page still answers CDP.
    async fn is_alive(&self) -> bool {
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        match tokio::time::timeout(Duration::from_secs(2), page.evaluate("1")).await {
            Ok(Ok(_)) => true,
            _ => false,
        }
    }

    async fn ensure_connected(&mut self, url_override: Option<&str>) -> anyhow::Result<()> {
        if self.browser.is_some() && self.page.is_some() {
            if self.is_alive().await {
                return Ok(());
            }
            tracing::warn!("CDP session stale; reconnecting");
            self.close().await;
        }
        let endpoint = url_override
            .map(|s| s.to_string())
            .or_else(cdp_url_from_env)
            .unwrap_or_else(|| "http://127.0.0.1:9222".into());

        // Try connect first (Playwright/Chrome already debugging).
        match chromiumoxide::Browser::connect(&endpoint).await {
            Ok((browser, handler)) => {
                self.attach(browser, handler, endpoint, None, None).await?;
                self.last_error = None;
                return Ok(());
            }
            Err(e) => {
                tracing::debug!(%endpoint, error = %e, "CDP connect failed; will try launch");
                self.last_error = Some(format!("connect {endpoint}: {e}"));
            }
        }

        // Launch chromium with remote debugging on an ephemeral port.
        let port = free_port()?;
        let bin = chromium_bin().ok_or_else(|| {
            anyhow::anyhow!(
                "no Chromium/Chrome on PATH and CDP connect to {endpoint} failed. \
                 Start Chrome with --remote-debugging-port=9222, or set PIRS_BROWSER_CDP_URL \
                 (Playwright: chromium.launch({{args:['--remote-debugging-port=9222']}}))"
            )
        })?;
        let user_data = tempfile::tempdir().map_err(|e| anyhow::anyhow!("user-data tempdir: {e}"))?;
        let user_data_path = user_data.path().to_path_buf();
        // Keep path; TempDir would delete on drop of local — persist via forget of guard only after success.
        let mut child = Command::new(&bin)
            .args([
                &format!("--remote-debugging-port={port}"),
                "--remote-allow-origins=*",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--headless=new",
                &format!("--user-data-dir={}", user_data_path.display()),
                "about:blank",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn chromium: {e}"))?;

        let endpoint = format!("http://127.0.0.1:{port}");
        // Wait for CDP to come up.
        let mut last_err = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            match chromiumoxide::Browser::connect(&endpoint).await {
                Ok((browser, handler)) => {
                    // Persist profile dir for cleanup on close (keep disables auto-delete).
                    let kept = user_data.keep();
                    self.attach(browser, handler, endpoint, Some(child), Some(kept))
                        .await?;
                    self.last_error = None;
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            }
            if let Ok(Some(status)) = child.try_wait() {
                let _ = std::fs::remove_dir_all(&user_data_path);
                anyhow::bail!("chromium exited early: {status:?}");
            }
        }
        let _ = child.kill();
        let _ = std::fs::remove_dir_all(&user_data_path);
        let msg = format!(
            "timed out connecting to launched chromium at {endpoint}: {:?}",
            last_err
        );
        self.last_error = Some(msg.clone());
        anyhow::bail!(msg);
    }

    async fn attach(
        &mut self,
        browser: chromiumoxide::Browser,
        mut handler: chromiumoxide::Handler,
        endpoint: String,
        child: Option<Child>,
        user_data_dir: Option<PathBuf>,
    ) -> anyhow::Result<()> {
        let h = tokio::spawn(async move {
            while let Some(evt) = handler.next().await {
                if evt.is_err() {
                    break;
                }
            }
        });
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| anyhow::anyhow!("new_page: {e}"))?;
        self.browser = Some(browser);
        self._handler = Some(h);
        self.page = Some(page);
        self.child = child;
        self.endpoint = Some(endpoint);
        self.user_data_dir = user_data_dir;
        Ok(())
    }

    async fn close(&mut self) {
        self.page = None;
        self.browser = None;
        if let Some(h) = self._handler.take() {
            h.abort();
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(dir) = self.user_data_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
        self.endpoint = None;
    }

}

fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CdpAction {
    /// Connect to CDP (env URL or launch Chromium).
    Connect,
    /// Navigate current page to URL.
    Goto,
    /// Return page text content (HTML stripped).
    Content,
    /// CSS selector click.
    Click,
    /// Type into focused element / selector.
    Type,
    /// Evaluate JS expression; return stringified result.
    Eval,
    /// Screenshot to path under cwd.
    Screenshot,
    /// Status of connection.
    Status,
    /// Disconnect / kill launched browser.
    Close,
}

#[derive(Deserialize, JsonSchema)]
struct CdpArgs {
    action: CdpAction,
    /// URL for connect override or goto.
    #[serde(default)]
    url: Option<String>,
    /// CSS selector for click/type.
    #[serde(default)]
    selector: Option<String>,
    /// Text for type.
    #[serde(default)]
    text: Option<String>,
    /// JS for eval.
    #[serde(default)]
    expression: Option<String>,
    /// Screenshot output path (default .pirs/cdp-shot.png).
    #[serde(default)]
    path: Option<String>,
    /// Max content chars (default 12000).
    #[serde(default)]
    max_chars: Option<usize>,
}

pub struct BrowserCdpTool {
    cwd: PathBuf,
}

impl BrowserCdpTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for BrowserCdpTool {
    fn name(&self) -> &str {
        "browser_cdp"
    }

    fn description(&self) -> &str {
        "Chrome DevTools Protocol browser automation (pure Rust via chromiumoxide). \
         Connect to Playwright/Chrome CDP (PIRS_BROWSER_CDP_URL) or auto-launch Chromium. \
         Actions: connect, goto, content, click, type, eval, screenshot, status, close."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(CdpArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("browser_cdp: full CDP control (goto/click/type/screenshot)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        if matches!(
            std::env::var("PIRS_BROWSER").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        ) {
            anyhow::bail!("browser tools disabled (PIRS_BROWSER=0)");
        }
        let args: CdpArgs = serde_json::from_value(ctx.args)?;
        let sess = session();
        let mut g = sess.lock().await;

        match args.action {
            CdpAction::Connect => {
                g.ensure_connected(args.url.as_deref()).await?;
                Ok(ToolOutput::text(format!(
                    "CDP connected endpoint={}",
                    g.endpoint.as_deref().unwrap_or("?")
                )))
            }
            CdpAction::Status => {
                let alive = if g.page.is_some() {
                    g.is_alive().await
                } else {
                    false
                };
                Ok(ToolOutput::text(format!(
                    "connected={} alive={} endpoint={:?} has_page={} launched_child={} last_error={:?}",
                    g.browser.is_some(),
                    alive,
                    g.endpoint,
                    g.page.is_some(),
                    g.child.is_some(),
                    g.last_error
                )))
            }
            CdpAction::Close => {
                g.close().await;
                Ok(ToolOutput::text("CDP session closed"))
            }
            CdpAction::Goto => {
                let url = args
                    .url
                    .ok_or_else(|| anyhow::anyhow!("goto requires url"))?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    anyhow::bail!("url must be http(s)");
                }
                g.ensure_connected(None).await?;
                let page = g.page.as_ref().unwrap();
                page.goto(&url)
                    .await
                    .map_err(|e| anyhow::anyhow!("goto: {e}"))?;
                // Wait for navigation to settle (load / short grace).
                let _ = page.wait_for_navigation().await;
                // Brief grace for late DOM paints when wait_for_navigation is a no-op.
                tokio::time::sleep(Duration::from_millis(150)).await;
                let title = page.get_title().await.ok().flatten().unwrap_or_default();
                Ok(ToolOutput::text(format!("navigated to {url} title={title:?}")))
            }
            CdpAction::Content => {
                g.ensure_connected(None).await?;
                let page = g.page.as_ref().unwrap();
                let html = page
                    .content()
                    .await
                    .map_err(|e| anyhow::anyhow!("content: {e}"))?;
                let max = args.max_chars.unwrap_or(12_000).min(50_000);
                let text = crate::web::truncate_chars(&html_to_text(&html), max);
                let url = page.url().await.ok().flatten().unwrap_or_default();
                Ok(ToolOutput::text(format!("URL: {url}\n\n{text}")))
            }
            CdpAction::Click => {
                let sel = args
                    .selector
                    .ok_or_else(|| anyhow::anyhow!("click requires selector"))?;
                g.ensure_connected(None).await?;
                let page = g.page.as_ref().unwrap();
                page.find_element(&sel)
                    .await
                    .map_err(|e| anyhow::anyhow!("find {sel}: {e}"))?
                    .click()
                    .await
                    .map_err(|e| anyhow::anyhow!("click: {e}"))?;
                Ok(ToolOutput::text(format!("clicked {sel}")))
            }
            CdpAction::Type => {
                let text = args
                    .text
                    .ok_or_else(|| anyhow::anyhow!("type requires text"))?;
                g.ensure_connected(None).await?;
                let page = g.page.as_ref().unwrap();
                // type_str lives on Element (chromiumoxide 0.9); focus selector first when given.
                let sel = args.selector.as_deref().unwrap_or("body");
                let el = page
                    .find_element(sel)
                    .await
                    .map_err(|e| anyhow::anyhow!("find {sel}: {e}"))?;
                if args.selector.is_some() {
                    el.click()
                        .await
                        .map_err(|e| anyhow::anyhow!("focus {sel}: {e}"))?;
                }
                el.type_str(&text)
                    .await
                    .map_err(|e| anyhow::anyhow!("type: {e}"))?;
                Ok(ToolOutput::text(format!("typed {} chars into {sel}", text.len())))
            }
            CdpAction::Eval => {
                let expr = args
                    .expression
                    .ok_or_else(|| anyhow::anyhow!("eval requires expression"))?;
                g.ensure_connected(None).await?;
                let page = g.page.as_ref().unwrap();
                let result = page
                    .evaluate(expr.as_str())
                    .await
                    .map_err(|e| anyhow::anyhow!("eval: {e}"))?;
                let s = result
                    .into_value::<serde_json::Value>()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| "(non-json result)".into());
                Ok(ToolOutput::text(s))
            }
            CdpAction::Screenshot => {
                g.ensure_connected(None).await?;
                let page = g.page.as_ref().unwrap();
                let rel = args
                    .path
                    .clone()
                    .unwrap_or_else(|| ".pirs/cdp-shot.png".into());
                let out = crate::paths::resolve_contained(&self.cwd, &rel)?;
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                page.save_screenshot(
                    chromiumoxide::page::ScreenshotParams::builder()
                        .full_page(true)
                        .build(),
                    &out,
                )
                .await
                .map_err(|e| anyhow::anyhow!("screenshot: {e}"))?;
                Ok(ToolOutput::text(format!(
                    "screenshot saved to {}",
                    out.display()
                )))
            }
        }
    }
}

pub fn cdp_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![Arc::new(BrowserCdpTool::new(cwd))]
}

#[cfg(test)]
mod tests {
    #[test]
    fn free_port_works() {
        let p = super::free_port().unwrap();
        assert!(p > 0);
    }

    #[test]
    fn cdp_url_env_keys() {
        // Smoke: helper is pure; env may be unset in CI.
        let _ = super::cdp_url_from_env();
    }
}
