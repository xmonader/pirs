//! Life tools: web_fetch / web_search with SSRF-safe URL policy.
//!
//! Shared by the `pirs` harness and `pirs-claw` (not claw-only).

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use serde_json::{json, Value};

/// Primary env; also accepts `PIRS_CLAW_ALLOW_PRIVATE_URLS` for back-compat.
pub const ALLOW_PRIVATE_URLS_ENV: &str = "PIRS_ALLOW_PRIVATE_URLS";
pub const HTTP_JSON_ENV: &str = "PIRS_HTTP_JSON";
pub const SEARCH_URL_ENV: &str = "PIRS_SEARCH_URL";

const MAX_FETCH_CHARS: usize = 30_000;
const MAX_SEARCH_CHARS: usize = 12_000;

pub fn life_tools(include_http_json: bool) -> Vec<Arc<dyn AgentTool>> {
    let mut v: Vec<Arc<dyn AgentTool>> = vec![Arc::new(WebFetchTool), Arc::new(WebSearchTool)];
    if include_http_json || http_json_enabled() {
        v.push(Arc::new(HttpJsonTool));
    }
    v
}

fn env_truthy(keys: &[&str]) -> bool {
    for k in keys {
        if let Ok(v) = std::env::var(k) {
            let v = v.trim().to_ascii_lowercase();
            if v == "1" || v == "true" || v == "yes" {
                return true;
            }
        }
    }
    false
}

fn http_json_enabled() -> bool {
    env_truthy(&[HTTP_JSON_ENV, "PIRS_CLAW_HTTP_JSON"])
}

fn allow_private() -> bool {
    env_truthy(&[ALLOW_PRIVATE_URLS_ENV, "PIRS_CLAW_ALLOW_PRIVATE_URLS"])
}

/// Reject SSRF-prone targets unless explicitly allowed.
pub fn url_allowed(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("empty URL".into());
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("scheme {scheme:?} not allowed (http/https only)"));
    }
    if allow_private() {
        return Ok(());
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() {
        return Err("URL missing host".into());
    }
    if host == "localhost"
        || host.ends_with(".localhost")
        || host == "metadata.google.internal"
        || host.ends_with(".local")
    {
        return Err(format!(
            "blocked host {host:?} (set {ALLOW_PRIVATE_URLS_ENV}=1 to allow private URLs)"
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip_is_private(&ip) {
            return Err(format!(
                "blocked private IP {ip} (set {ALLOW_PRIVATE_URLS_ENV}=1 to allow)"
            ));
        }
    }
    Ok(())
}

fn ip_is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.octets()[0] == 169 && v4.octets()[1] == 254
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unspecified(),
    }
}

/// Strip tags lightly and collapse whitespace for model context.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !in_tag && lower[i..].starts_with("<script") {
            in_script = true;
        }
        if in_script && lower[i..].starts_with("</script") {
            in_script = false;
        }
        let c = bytes[i] as char;
        if c == '<' {
            in_tag = true;
            i += 1;
            continue;
        }
        if c == '>' {
            in_tag = false;
            i += 1;
            continue;
        }
        if !in_tag && !in_script {
            out.push(c);
        }
        i += 1;
    }
    collapse_ws(&out)
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            prev_space = false;
            out.push(c);
        }
    }
    out.trim().to_string()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let t: String = s.chars().take(max).collect();
    format!("{t}\n…[truncated {} chars]", count - max)
}

struct WebFetchTool;

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a public http(s) URL and return text content (HTML stripped, truncated). \
         Private/localhost URLs are blocked by default."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http(s) URL to fetch" }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let url = ctx
            .args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("url required"))?;
        url_allowed(url).map_err(|e| anyhow::anyhow!(e))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("pirs-claw")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let resp = client.get(url).send().await?;
        let status = resp.status();
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await?;
        let text = if ct.contains("html") || body.trim_start().starts_with('<') {
            html_to_text(&body)
        } else {
            body
        };
        let text = truncate_chars(&text, MAX_FETCH_CHARS);
        Ok(ToolOutput::text(format!(
            "HTTP {status}\nContent-Type: {ct}\n\n{text}"
        )))
    }
}

struct WebSearchTool;

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web (DuckDuckGo lite by default) and return result snippets."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "search query" }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let q = ctx
            .args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("query required"))?;
        let encoded: String = urlencoding_minimal(q);
        let url = if let Ok(tmpl) = std::env::var(SEARCH_URL_ENV)
            .or_else(|_| std::env::var("PIRS_CLAW_SEARCH_URL"))
        {
            tmpl.replace("{query}", &encoded)
        } else {
            format!("https://lite.duckduckgo.com/lite/?q={encoded}")
        };
        url_allowed(&url).map_err(|e| anyhow::anyhow!(e))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("Mozilla/5.0 (compatible; pirs-claw)")
            .build()?;
        let body = client.get(&url).send().await?.text().await?;
        let text = truncate_chars(&html_to_text(&body), MAX_SEARCH_CHARS);
        Ok(ToolOutput::text(format!("Search results for {q:?}:\n\n{text}")))
    }
}

struct HttpJsonTool;

#[async_trait]
impl AgentTool for HttpJsonTool {
    fn name(&self) -> &str {
        "http_json"
    }

    fn description(&self) -> &str {
        "HTTP GET/POST JSON to a public URL (opt-in tool). Body truncated."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "method": { "type": "string", "enum": ["GET", "POST"], "default": "GET" },
                "body": { "type": "string", "description": "JSON body for POST" }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let url = ctx
            .args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("url required"))?;
        url_allowed(url).map_err(|e| anyhow::anyhow!(e))?;
        let method = ctx
            .args
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("pirs-claw")
            .build()?;
        let resp = if method.eq_ignore_ascii_case("POST") {
            let body = ctx.args.get("body").and_then(|v| v.as_str()).unwrap_or("{}");
            client
                .post(url)
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await?
        } else {
            client.get(url).send().await?
        };
        let status = resp.status();
        let text = truncate_chars(&resp.text().await?, MAX_FETCH_CHARS);
        Ok(ToolOutput::text(format!("HTTP {status}\n\n{text}")))
    }
}

fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_localhost_and_private() {
        assert!(url_allowed("http://localhost/x").is_err());
        assert!(url_allowed("http://127.0.0.1/").is_err());
        assert!(url_allowed("http://10.0.0.1/").is_err());
        assert!(url_allowed("http://169.254.169.254/latest").is_err());
        assert!(url_allowed("https://example.com/a").is_ok());
    }

    #[test]
    fn html_strip_basic() {
        let t = html_to_text("<html><script>evil()</script><p>Hello <b>world</b></p></html>");
        assert!(t.contains("Hello"));
        assert!(t.contains("world"));
        assert!(!t.contains("evil"));
    }

    #[test]
    fn truncate_works() {
        let s = "abcd";
        assert_eq!(truncate_chars(s, 10), "abcd");
        assert!(truncate_chars(s, 2).starts_with("ab"));
        assert!(truncate_chars(s, 2).contains("truncated"));
    }
}
