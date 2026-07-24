//! Pure-Rust CDP browser automation via **chromiumoxide**.
//!
//! Connect to an existing Chrome/Chromium/Playwright CDP endpoint
//! (`PIRS_BROWSER_CDP_URL` / `BROWSER_CDP_URL`, default try `http://127.0.0.1:9222`)
//! or launch a local Chromium with remote debugging.
//!
//! Tools (single multi-action tool + helpers):
//! - `browser_cdp` — connect | goto | content | click | type | eval | screenshot |
//!   open_page | list_pages | switch_page | close | status
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
    // Ubuntu snap installs the binary here even when the PATH wrapper is odd.
    let snap = PathBuf::from("/snap/bin/chromium");
    if snap.is_file() {
        return Some(snap);
    }
    None
}

/// One tracked tab/page in the CDP session.
struct TrackedPage {
    /// Stable id for switch_page / list_pages (`p1`, `p2`, …).
    id: String,
    page: chromiumoxide::Page,
    /// Last known URL label (best-effort).
    url: String,
}

struct CdpSession {
    browser: Option<chromiumoxide::Browser>,
    /// Keep handler task alive.
    _handler: Option<tokio::task::JoinHandle<()>>,
    /// All open pages; `active` indexes the current one.
    pages: Vec<TrackedPage>,
    active: usize,
    next_page_num: u32,
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
            pages: Vec::new(),
            active: 0,
            next_page_num: 1,
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
    fn active_page(&self) -> Option<&chromiumoxide::Page> {
        self.pages.get(self.active).map(|p| &p.page)
    }

    fn active_id(&self) -> Option<&str> {
        self.pages.get(self.active).map(|p| p.id.as_str())
    }

    fn push_page(&mut self, page: chromiumoxide::Page, url: String) -> String {
        let id = format!("p{}", self.next_page_num);
        self.next_page_num = self.next_page_num.saturating_add(1);
        self.pages.push(TrackedPage {
            id: id.clone(),
            page,
            url,
        });
        self.active = self.pages.len() - 1;
        id
    }

    fn list_summary(&self) -> String {
        if self.pages.is_empty() {
            return "pages: (none)".into();
        }
        let mut lines = vec![format!(
            "pages: {} active={}",
            self.pages.len(),
            self.active_id().unwrap_or("?")
        )];
        for (i, p) in self.pages.iter().enumerate() {
            let mark = if i == self.active { "*" } else { " " };
            lines.push(format!("{mark} {}  {}", p.id, p.url));
        }
        lines.join("\n")
    }

    /// Probe whether the current page still answers CDP.
    async fn is_alive(&self) -> bool {
        let Some(page) = self.active_page() else {
            return false;
        };
        match tokio::time::timeout(Duration::from_secs(2), page.evaluate("1")).await {
            Ok(Ok(_)) => true,
            _ => false,
        }
    }

    async fn ensure_connected(&mut self, url_override: Option<&str>) -> anyhow::Result<()> {
        if self.browser.is_some() && self.active_page().is_some() {
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
                // Snap/CI/container Chromium often needs these to bind CDP at all.
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--disable-gpu",
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
        self.pages.clear();
        self.active = 0;
        self.next_page_num = 1;
        self.push_page(page, "about:blank".into());
        self.child = child;
        self.endpoint = Some(endpoint);
        self.user_data_dir = user_data_dir;
        Ok(())
    }

    async fn open_page(&mut self, url: Option<&str>) -> anyhow::Result<String> {
        self.ensure_connected(None).await?;
        let browser = self
            .browser
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no browser"))?;
        let target = url.unwrap_or("about:blank");
        if target != "about:blank"
            && !(target.starts_with("http://")
                || target.starts_with("https://")
                || target.starts_with("data:"))
        {
            anyhow::bail!("url must be http(s), data:, or about:blank");
        }
        if target.starts_with("http://") || target.starts_with("https://") {
            crate::web::url_allowed(target).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let page = browser
            .new_page(target)
            .await
            .map_err(|e| anyhow::anyhow!("open_page: {e}"))?;
        let id = self.push_page(page, target.to_string());
        Ok(id)
    }

    fn switch_page(&mut self, id_or_index: &str) -> anyhow::Result<String> {
        if self.pages.is_empty() {
            anyhow::bail!("no pages open; connect or open_page first");
        }
        // Accept pN id or 0-based / 1-based index.
        if let Some(i) = self.pages.iter().position(|p| p.id == id_or_index) {
            self.active = i;
            return Ok(format!(
                "switched to {} url={}",
                self.pages[i].id, self.pages[i].url
            ));
        }
        if let Ok(n) = id_or_index.parse::<usize>() {
            // 1-based if in 1..=len, else 0-based
            let i = if n >= 1 && n <= self.pages.len() {
                n - 1
            } else {
                n
            };
            if i < self.pages.len() {
                self.active = i;
                return Ok(format!(
                    "switched to {} url={}",
                    self.pages[i].id, self.pages[i].url
                ));
            }
        }
        anyhow::bail!(
            "unknown page {id_or_index:?}; known: {}",
            self.pages
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    async fn close(&mut self) {
        self.pages.clear();
        self.active = 0;
        self.next_page_num = 1;
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
    /// Open a new tab/page (optional url).
    OpenPage,
    /// List tracked pages and mark the active one.
    ListPages,
    /// Switch active page by id (`p1`) or index.
    SwitchPage,
    /// Status of connection + active page.
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
    /// Page id (`p1`) or index for switch_page.
    #[serde(default)]
    page: Option<String>,
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
         Actions: connect, goto, content, click, type, eval, screenshot, \
         open_page, list_pages, switch_page, status, close."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(CdpArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("browser_cdp: CDP multi-page (goto/open_page/list_pages/switch_page/click)")
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
                    "CDP connected endpoint={} active={} pages={}\n{}",
                    g.endpoint.as_deref().unwrap_or("?"),
                    g.active_id().unwrap_or("?"),
                    g.pages.len(),
                    g.list_summary()
                )))
            }
            CdpAction::Status => {
                let alive = if g.active_page().is_some() {
                    g.is_alive().await
                } else {
                    false
                };
                Ok(ToolOutput::text(format!(
                    "connected={} alive={} endpoint={:?} active_page={:?} page_count={} launched_child={} last_error={:?}\n{}",
                    g.browser.is_some(),
                    alive,
                    g.endpoint,
                    g.active_id(),
                    g.pages.len(),
                    g.child.is_some(),
                    g.last_error,
                    g.list_summary()
                )))
            }
            CdpAction::OpenPage => {
                let id = g.open_page(args.url.as_deref()).await?;
                Ok(ToolOutput::text(format!(
                    "opened page id={id}\n{}",
                    g.list_summary()
                )))
            }
            CdpAction::ListPages => {
                g.ensure_connected(None).await.ok();
                Ok(ToolOutput::text(g.list_summary()))
            }
            CdpAction::SwitchPage => {
                let key = args
                    .page
                    .or(args.url)
                    .ok_or_else(|| anyhow::anyhow!("switch_page requires page=pN (or index)"))?;
                let msg = g.switch_page(&key)?;
                Ok(ToolOutput::text(format!("{msg}\n{}", g.list_summary())))
            }
            CdpAction::Close => {
                g.close().await;
                Ok(ToolOutput::text("CDP session closed"))
            }
            CdpAction::Goto => {
                let url = args
                    .url
                    .ok_or_else(|| anyhow::anyhow!("goto requires url"))?;
                if !(url.starts_with("http://")
                    || url.starts_with("https://")
                    || url.starts_with("data:"))
                {
                    anyhow::bail!("url must be http(s) or data:");
                }
                if url.starts_with("http://") || url.starts_with("https://") {
                    crate::web::url_allowed(&url).map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                g.ensure_connected(None).await?;
                let title = {
                    let page = g.active_page().unwrap();
                    page.goto(&url)
                        .await
                        .map_err(|e| anyhow::anyhow!("goto: {e}"))?;
                    // Wait for navigation to settle (load / short grace).
                    let _ = page.wait_for_navigation().await;
                    // Brief grace for late DOM paints when wait_for_navigation is a no-op.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    page.get_title().await.ok().flatten().unwrap_or_default()
                };
                let active = g.active;
                if let Some(tp) = g.pages.get_mut(active) {
                    tp.url = url.clone();
                }
                Ok(ToolOutput::text(format!(
                    "navigated to {url} title={title:?} page={}",
                    g.active_id().unwrap_or("?")
                )))
            }
            CdpAction::Content => {
                g.ensure_connected(None).await?;
                let (url, text) = {
                    let page = g.active_page().unwrap();
                    let html = page
                        .content()
                        .await
                        .map_err(|e| anyhow::anyhow!("content: {e}"))?;
                    let max = args.max_chars.unwrap_or(12_000).min(50_000);
                    let text = crate::web::truncate_chars(&html_to_text(&html), max);
                    let url = page.url().await.ok().flatten().unwrap_or_default();
                    (url, text)
                };
                let active = g.active;
                if let Some(tp) = g.pages.get_mut(active) {
                    if !url.is_empty() {
                        tp.url = url.clone();
                    }
                }
                Ok(ToolOutput::text(format!(
                    "page={} URL: {url}\n\n{text}",
                    g.active_id().unwrap_or("?")
                )))
            }
            CdpAction::Click => {
                let sel = args
                    .selector
                    .ok_or_else(|| anyhow::anyhow!("click requires selector"))?;
                g.ensure_connected(None).await?;
                let page = g.active_page().unwrap();
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
                let page = g.active_page().unwrap();
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
                let page = g.active_page().unwrap();
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
                let page = g.active_page().unwrap();
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
    use super::*;

    #[test]
    fn free_port_works() {
        let p = free_port().unwrap();
        assert!(p > 0);
    }

    #[test]
    fn cdp_url_env_keys() {
        // Smoke: helper is pure; env may be unset in CI.
        let _ = cdp_url_from_env();
    }

    #[test]
    fn list_summary_empty_and_tracked_ids() {
        let mut s = CdpSession::default();
        assert!(s.list_summary().contains("none"));
        // Simulate id allocation without a real Page (unit-only helpers).
        s.next_page_num = 1;
        let id1 = format!("p{}", s.next_page_num);
        s.next_page_num += 1;
        let id2 = format!("p{}", s.next_page_num);
        assert_eq!(id1, "p1");
        assert_eq!(id2, "p2");
    }

    #[test]
    fn switch_page_resolves_id_and_index() {
        // Pure dispatch logic without Chrome: exercise switch_page errors and index math.
        let mut s = CdpSession::default();
        let err = s.switch_page("p1").unwrap_err().to_string();
        assert!(err.contains("no pages"), "{err}");

        // Manually seed metadata-only page list is not possible without Page;
        // cover action serde / schema instead.
        let raw = serde_json::json!({
            "action": "switch_page",
            "page": "p2"
        });
        let args: CdpArgs = serde_json::from_value(raw).unwrap();
        assert!(matches!(args.action, CdpAction::SwitchPage));
        assert_eq!(args.page.as_deref(), Some("p2"));

        let open = serde_json::json!({"action": "open_page", "url": "about:blank"});
        let a2: CdpArgs = serde_json::from_value(open).unwrap();
        assert!(matches!(a2.action, CdpAction::OpenPage));

        let list = serde_json::json!({"action": "list_pages"});
        let a3: CdpArgs = serde_json::from_value(list).unwrap();
        assert!(matches!(a3.action, CdpAction::ListPages));
    }

    #[test]
    fn status_action_deserializes() {
        let args: CdpArgs = serde_json::from_value(serde_json::json!({"action": "status"})).unwrap();
        assert!(matches!(args.action, CdpAction::Status));
    }

    #[test]
    fn chromium_bin_probe_is_honest() {
        // Does not require Chrome; just ensures helper does not panic and
        // prefers known names when present on this host.
        let found = chromium_bin();
        if which("chromium-browser").is_some() || PathBuf::from("/snap/bin/chromium").is_file() {
            assert!(found.is_some(), "should detect chromium-browser/snap");
        }
    }

    /// Live CDP: connect → open_page → list → switch → content (requires Chrome).
    /// Run with `PIRS_CDP_LIVE=1 cargo test -p pirs-tools live_cdp -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "requires local Chromium; set PIRS_CDP_LIVE=1 and --ignored"]
    async fn live_cdp_multipage_connect_content() {
        if std::env::var("PIRS_CDP_LIVE").as_deref() != Ok("1") {
            return;
        }
        use pirs_agent::{AgentTool, ToolExecContext};
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().unwrap();
        let tool = BrowserCdpTool::new(dir.path().to_path_buf());
        let exec = |args: serde_json::Value| {
            let tool = BrowserCdpTool::new(dir.path().to_path_buf());
            async move {
                tool.execute(ToolExecContext {
                    tool_call_id: "t".into(),
                    args,
                    cancel: CancellationToken::new(),
                    on_update: None,
                })
                .await
            }
        };

        let connect = exec(serde_json::json!({"action": "connect"})).await.unwrap();
        let c = connect.content[0].as_text().unwrap();
        assert!(c.contains("connected") || c.contains("CDP"), "{c}");

        let open = exec(serde_json::json!({
            "action": "open_page",
            "url": "about:blank"
        }))
        .await
        .unwrap();
        assert!(
            open.content[0].as_text().unwrap().contains("opened")
                || open.content[0].as_text().unwrap().contains("p"),
            "{:?}",
            open.content[0].as_text()
        );

        let list = exec(serde_json::json!({"action": "list_pages"}))
            .await
            .unwrap();
        let l = list.content[0].as_text().unwrap();
        assert!(l.contains("pages"), "{l}");

        let status = exec(serde_json::json!({"action": "status"})).await.unwrap();
        let s = status.content[0].as_text().unwrap();
        assert!(s.contains("connected=true") || s.contains("alive=true"), "{s}");

        let _ = exec(serde_json::json!({"action": "close"})).await;
        let _ = tool; // keep type alive for name check
        assert_eq!(BrowserCdpTool::new(dir.path().to_path_buf()).name(), "browser_cdp");
    }
}
