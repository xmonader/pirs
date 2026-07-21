//! Background research job — long-running agent-style web investigation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::web::{html_to_text, truncate_chars};

#[derive(Deserialize, JsonSchema)]
struct ResearchArgs {
    /// Research question / topic.
    query: String,
    /// Optional seed URLs (http/https).
    #[serde(default)]
    urls: Vec<String>,
    /// Max pages to fetch (default 3, max 6).
    #[serde(default)]
    max_pages: Option<usize>,
}

/// Synchronous multi-fetch research digest (stores under `.pirs/research/`).
pub struct ResearchTool {
    cwd: PathBuf,
}

impl ResearchTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for ResearchTool {
    fn name(&self) -> &str {
        "research"
    }

    fn description(&self) -> &str {
        "Background-style web research: fetch several pages (and optional seed URLs), \
         extract text, write a digest under .pirs/research/, return a summary. \
         Use for multi-source investigation (not a single web_fetch)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ResearchArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("research: multi-page web research digest → .pirs/research/")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: ResearchArgs = serde_json::from_value(ctx.args)?;
        if args.query.trim().is_empty() {
            anyhow::bail!("research requires query");
        }
        let max = args.max_pages.unwrap_or(3).clamp(1, 6);
        let client = crate::web::ssrf_safe_client(25).map_err(|e| anyhow::anyhow!(e))?;

        let mut urls = args.urls;
        // If no seeds, use DuckDuckGo HTML search for a few result links.
        if urls.is_empty() {
            let q = urlencoding_simple(&args.query);
            let search_url = format!("https://html.duckduckgo.com/html/?q={q}");
            if let Ok(resp) = client.get(&search_url).send().await {
                if let Ok(body) = resp.text().await {
                    urls.extend(extract_http_links(&body).into_iter().take(max));
                }
            }
        }
        urls.truncate(max);
        if urls.is_empty() {
            anyhow::bail!(
                "no URLs to research — pass urls: [\"https://...\"] or ensure network for search"
            );
        }

        let mut digest = format!("# Research: {}\n\n", args.query.trim());
        for (i, url) in urls.iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                anyhow::bail!("research cancelled");
            }
            digest.push_str(&format!("## Source {}\nURL: {url}\n\n", i + 1));
            match fetch_page(&client, url).await {
                Ok(text) => {
                    digest.push_str(&truncate_chars(&text, 4000));
                    digest.push_str("\n\n");
                }
                Err(e) => {
                    digest.push_str(&format!("(fetch failed: {e})\n\n"));
                }
            }
        }

        let dir = self.cwd.join(".pirs").join("research");
        std::fs::create_dir_all(&dir)?;
        let name = format!(
            "research_{}.md",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let path = dir.join(&name);
        std::fs::write(&path, &digest)?;

        let preview = truncate_chars(&digest, 6000);
        Ok(ToolOutput::text(format!(
            "Research digest saved to {}\n\n{preview}",
            path.display()
        )))
    }
}

async fn fetch_page(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    // Shared SSRF policy (scheme, private/metadata, DNS, redirects via client).
    crate::web::url_allowed(url).map_err(|e| anyhow::anyhow!("{e}"))?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }
    let body = resp.text().await?;
    Ok(html_to_text(&body))
}

fn extract_http_links(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("http") {
        let slice = &rest[i..];
        let end = slice
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>')
            .unwrap_or(slice.len().min(200));
        let u = slice[..end].trim_end_matches(['.', ',', ')', ']']).to_string();
        if (u.starts_with("http://") || u.starts_with("https://")) && !out.contains(&u) {
            // Skip duckduckgo chrome
            if !u.contains("duckduckgo.com") {
                out.push(u);
            }
        }
        rest = &slice[1..];
        if out.len() >= 10 {
            break;
        }
    }
    out
}

fn urlencoding_simple(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            b' ' => o.push_str("%20"),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

pub fn research_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![Arc::new(ResearchTool::new(cwd))]
}
