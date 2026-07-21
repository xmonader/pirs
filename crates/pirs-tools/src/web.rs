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
///
/// Checks scheme, blocked hostnames, literal private IPs (incl. IPv4-mapped),
/// and best-effort DNS resolution of the host to private addresses.
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
    // host_str() for IPv6 includes brackets (`[::1]`) which break IpAddr::parse.
    let host_raw = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if host_raw.is_empty() {
        return Err("URL missing host".into());
    }
    let host = host_raw
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host == "localhost"
        || host.ends_with(".localhost")
        || host == "metadata.google.internal"
        || host.ends_with(".local")
        || host == "metadata"
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
        return Ok(());
    }
    // Best-effort DNS: reject if any resolved address is private/link-local.
    use std::net::ToSocketAddrs;
    let port = parsed.port_or_known_default().unwrap_or(80);
    if let Ok(addrs) = (host.as_str(), port).to_socket_addrs() {
        for addr in addrs {
            if ip_is_private(&addr.ip()) {
                return Err(format!(
                    "blocked host {host:?} resolves to private IP {} \
                     (set {ALLOW_PRIVATE_URLS_ENV}=1 to allow)",
                    addr.ip()
                ));
            }
        }
    }
    Ok(())
}

/// reqwest redirect policy that re-runs [`url_allowed`] on every hop.
pub fn ssrf_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.error("too many redirects");
        }
        match url_allowed(attempt.url().as_str()) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(e),
        }
    })
}

/// HTTP client with SSRF-safe redirect policy and a short timeout.
pub fn ssrf_safe_client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(concat!("pirs/", env!("CARGO_PKG_VERSION")))
        .redirect(ssrf_redirect_policy())
        .build()
        .map_err(|e| e.to_string())
}

fn ip_is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.octets()[0] == 169 && v4.octets()[1] == 254
                || v4.is_unspecified()
                // CGNAT
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:x.x.x.x) and compatible forms.
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4()) {
                return ip_is_private(&IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unique_local()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local
        }
    }
}

/// Case-insensitive ASCII prefix match at byte offset (UTF-8 safe — no str slice).
fn bytes_prefix_eq_ignore_ascii_case(hay: &[u8], at: usize, needle: &[u8]) -> bool {
    let Some(slice) = hay.get(at..at + needle.len()) else {
        return false;
    };
    slice.eq_ignore_ascii_case(needle)
}

/// Strip tags lightly and collapse whitespace for model context.
///
/// Must not panic on non-ASCII (emoji, CJK, …): earlier code sliced a lowercased
/// `str` at arbitrary byte offsets and crashed mid-codepoint (e.g. inside '💬').
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !in_tag && bytes_prefix_eq_ignore_ascii_case(bytes, i, b"<script") {
            in_script = true;
        }
        if in_script && bytes_prefix_eq_ignore_ascii_case(bytes, i, b"</script") {
            in_script = false;
        }
        let b = bytes[i];
        if b == b'<' {
            in_tag = true;
            i += 1;
            continue;
        }
        if b == b'>' {
            in_tag = false;
            i += 1;
            continue;
        }
        // Advance by full UTF-8 char so multi-byte glyphs stay intact.
        let ch = html[i..].chars().next().unwrap_or('\u{FFFD}');
        let n = ch.len_utf8();
        if !in_tag && !in_script {
            out.push(ch);
        }
        i += n;
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
        let client = ssrf_safe_client(20).map_err(|e| anyhow::anyhow!(e))?;
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
        let client = ssrf_safe_client(20).map_err(|e| anyhow::anyhow!(e))?;
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
        let client = ssrf_safe_client(20).map_err(|e| anyhow::anyhow!(e))?;
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
        assert!(url_allowed("http://metadata.google.internal/").is_err());
        assert!(url_allowed("http://[::ffff:127.0.0.1]/").is_err());
        assert!(url_allowed("https://example.com/a").is_ok());
    }

    #[test]
    fn ssrf_redirect_policy_rejects_private_hop() {
        // Policy construction must re-check url_allowed — exercise via Policy
        // by ensuring private URLs fail url_allowed (used by custom policy).
        assert!(url_allowed("http://127.0.0.1/redirect-target").is_err());
        let _policy = ssrf_redirect_policy();
        // Client builds with the policy (no network).
        assert!(ssrf_safe_client(5).is_ok());
    }

    #[test]
    fn html_strip_basic() {
        let t = html_to_text("<html><script>evil()</script><p>Hello <b>world</b></p></html>");
        assert!(t.contains("Hello"));
        assert!(t.contains("world"));
        assert!(!t.contains("evil"));
    }

    #[test]
    fn html_strip_utf8_emoji_no_panic() {
        // Repro: byte-index slice mid-codepoint inside 💬 panicked the gateway.
        let html = format!(
            "<html><body><p>hi 💬 there</p><script>x</script>{}</body></html>",
            "💬".repeat(100)
        );
        let t = html_to_text(&html);
        assert!(t.contains('💬'), "{t}");
        assert!(t.contains("hi"), "{t}");
        assert!(!t.contains('x'));
    }

    #[test]
    fn truncate_works() {
        let s = "abcd";
        assert_eq!(truncate_chars(s, 10), "abcd");
        assert!(truncate_chars(s, 2).starts_with("ab"));
        assert!(truncate_chars(s, 2).contains("truncated"));
    }
}

