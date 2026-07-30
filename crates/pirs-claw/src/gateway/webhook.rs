//! HTTP webhooks: Discord / Slack / WhatsApp ingress + signature verify.

use crate::channel::{InboundMessage, CHANNEL_DISCORD, CHANNEL_SLACK, CHANNEL_WHATSAPP};
use crate::pairing::PairingAllowlist;
use crate::GatewayReply;

use super::allow::require_allowlist;
use super::bind::{webhook_bind_host, webhook_socket_addr};
use super::outbound::{send_discord, send_slack, send_whatsapp};
use super::MessageHandler;

type SendFuture = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;

pub(super) async fn run_webhook_listener(
    channel: &'static str,
    port_env: &str,
    default_port: u16,
    allowlist: &PairingAllowlist,
    extract: fn(&serde_json::Value) -> Option<(String, String)>,
    send: fn(&str, &str) -> SendFuture,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    require_allowlist(allowlist, channel)?;
    let port: u16 = std::env::var(port_env)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_port);
    let addr = webhook_socket_addr(port);
    let host = webhook_bind_host();
    if host == "0.0.0.0" || host == "::" {
        eprintln!(
            "[pirs-claw] WARNING: webhook bound publicly on {addr} — ensure firewall + pairing"
        );
    }
    // Public bind without a shared secret is remote prompt injection (M-33).
    let public = host == "0.0.0.0" || host == "::";
    if public && webhook_secret_for(channel).is_none() {
        anyhow::bail!(
            "{channel}: public webhook bind ({addr}) requires PIRS_WEBHOOK_SECRET \
             (or PIRS_{}_WEBHOOK_SECRET). Refusing to listen open.",
            channel.to_ascii_uppercase()
        );
    }
    // Optional hard require even on localhost: PIRS_WEBHOOK_REQUIRE_SECRET=1
    if matches!(
        std::env::var("PIRS_WEBHOOK_REQUIRE_SECRET").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) && webhook_secret_for(channel).is_none()
    {
        anyhow::bail!("{channel}: PIRS_WEBHOOK_REQUIRE_SECRET=1 but no webhook secret configured");
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "[pirs-claw gateway] {channel} webhook listening on {addr} (POST / JSON body; default localhost)"
    );
    let allowlist = allowlist.clone();
    loop {
        let (mut sock, _) = listener.accept().await?;
        let allowlist = allowlist.clone();
        let on_message = on_message.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let raw = match read_http_request(&mut sock).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[{channel}] bad HTTP request: {e}");
                    let _ = sock
                        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
            };
            let first_line = raw.lines().next().unwrap_or("");
            // WhatsApp / Meta hub.challenge verification (GET).
            if first_line.starts_with("GET ") {
                if let Some(q) = first_line.split_whitespace().nth(1) {
                    if let Some(challenge) = whatsapp_verify_challenge(q) {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                            challenge.len(),
                            challenge
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
            // Find body after \r\n\r\n (read_http_request already assembled full body).
            let (headers, body) = match raw.split_once("\r\n\r\n") {
                Some((h, b)) => (h, b),
                None => ("", raw.as_str()),
            };
            // Signature gate when a shared secret is configured (Opus §2.5).
            // Without a secret we still rely on pairing allowlist + localhost
            // bind default; with a secret, unsigned POSTs are rejected.
            if let Err(reason) = verify_webhook_signature(channel, headers, body) {
                eprintln!("[{channel}] webhook signature rejected: {reason}");
                let _ = sock
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
                let _ = sock
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            };
            // Slack URL verification challenge (JSON body)
            if let Some(challenge) = v.get("challenge").and_then(|c| c.as_str()) {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                    challenge.len(),
                    challenge
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                return;
            }
            let Some((peer, text)) = extract(&v) else {
                let _ = sock
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            };
            if !allowlist.is_allowed(&peer) {
                let _ = sock
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
            let inbound = InboundMessage {
                channel_id: channel.into(),
                peer_id: peer.clone(),
                text,
                ts: crate::channel::now_secs_pub(),
            };
            let reply = match on_message(inbound).await {
                Ok(r) => r,
                Err(e) => GatewayReply::text(format!("error: {e}")),
            };
            // Webhooks: text only for now (no native multi-channel file send here).
            if let Err(e) = send(&peer, &reply.text).await {
                eprintln!("[{channel}] send error: {e}");
            }
            if !reply.attachments.is_empty() {
                eprintln!(
                    "[{channel}] {} attachment(s) staged but only Telegram delivers files today",
                    reply.attachments.len()
                );
            }
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                .await;
        });
    }
}

/// Max webhook body we will buffer (1 MiB). Larger requests are rejected.
pub const WEBHOOK_MAX_BODY: usize = 1024 * 1024;

/// Read a full HTTP/1.x request (headers + body via Content-Length).
///
/// Replaces a single 64 KiB `read()` that mis-parsed fragmented or large
/// bodies behind reverse proxies (review M-32).
pub async fn read_http_request(sock: &mut tokio::net::TcpStream) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    // Header phase: read until \r\n\r\n
    loop {
        if buf.len() > 64 * 1024 {
            anyhow::bail!("HTTP headers too large");
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("connection closed before headers complete");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            // pos is start of \r\n\r\n; headers end at pos, body starts at pos+4.
            let header_bytes = &buf[..pos];
            let headers = String::from_utf8_lossy(header_bytes);
            let content_len = parse_content_length(&headers).unwrap_or(0);
            if content_len > WEBHOOK_MAX_BODY {
                anyhow::bail!("body too large ({content_len} > {WEBHOOK_MAX_BODY})");
            }
            let mut body = buf[pos + 4..].to_vec();
            while body.len() < content_len {
                let n = sock.read(&mut tmp).await?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
                if body.len() > WEBHOOK_MAX_BODY {
                    anyhow::bail!("body exceeded max while reading");
                }
            }
            if content_len > 0 {
                body.truncate(content_len);
            }
            let mut raw = header_bytes.to_vec();
            raw.extend_from_slice(b"\r\n\r\n");
            raw.extend_from_slice(&body);
            return Ok(String::from_utf8_lossy(&raw).into_owned());
        }
    }
}

pub(super) fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

pub(super) fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Slack request timestamps older/newer than this are rejected (replay window).
pub const SLACK_TIMESTAMP_SKEW_SECS: i64 = 5 * 60;

/// Verify webhook authenticity when `PIRS_WEBHOOK_SECRET` (or channel-specific
/// env) is set. Supports:
/// - generic: `X-Pirs-Signature: sha256=<hex>` HMAC-SHA256 of body
/// - Slack: `X-Slack-Signature` + `X-Slack-Request-Timestamp` (v0 scheme, freshness)
/// - GitHub-style: `X-Hub-Signature-256: sha256=<hex>`
///
/// If no secret is configured, returns Ok (pairing allowlist remains the gate).
/// If a secret is configured and the signature is missing/wrong, returns Err.
pub fn verify_webhook_signature(channel: &str, headers: &str, body: &str) -> Result<(), String> {
    verify_webhook_signature_at(
        channel,
        headers,
        body,
        crate::channel::now_secs_pub() as i64,
    )
}

/// Same as [`verify_webhook_signature`] with injectable clock (tests).
pub fn verify_webhook_signature_at(
    channel: &str,
    headers: &str,
    body: &str,
    now_secs: i64,
) -> Result<(), String> {
    let secret = webhook_secret_for(channel);
    let Some(secret) = secret else {
        return Ok(());
    };
    let hdrs = parse_http_headers(headers);
    // Prefer channel-native headers, then generic.
    if let Some(sig) = hdrs.get("x-slack-signature").cloned() {
        let ts = hdrs
            .get("x-slack-request-timestamp")
            .ok_or_else(|| "slack signature requires X-Slack-Request-Timestamp".to_string())?;
        let ts_i: i64 = ts
            .parse()
            .map_err(|_| "invalid X-Slack-Request-Timestamp".to_string())?;
        if (now_secs - ts_i).abs() > SLACK_TIMESTAMP_SKEW_SECS {
            return Err(format!(
                "slack timestamp skew too large (|{now_secs}-{ts_i}| > {SLACK_TIMESTAMP_SKEW_SECS}s)"
            ));
        }
        // Slack: v0:{ts}:{body}
        let base = format!("v0:{ts}:{body}");
        if hmac_sha256_hex_eq(secret.as_bytes(), base.as_bytes(), &sig) {
            return Ok(());
        }
        return Err("slack/hmac signature mismatch".into());
    }
    if let Some(sig) = hdrs
        .get("x-hub-signature-256")
        .cloned()
        .or_else(|| hdrs.get("x-pirs-signature").cloned())
    {
        if hmac_sha256_hex_eq(secret.as_bytes(), body.as_bytes(), &sig) {
            return Ok(());
        }
        return Err("hmac signature mismatch".into());
    }
    Err("webhook secret configured but no X-Pirs-Signature / X-Hub-Signature-256 / X-Slack-Signature header".into())
}

fn webhook_secret_for(channel: &str) -> Option<String> {
    let keys = [
        format!("PIRS_{}_WEBHOOK_SECRET", channel.to_ascii_uppercase()),
        "PIRS_WEBHOOK_SECRET".into(),
        "WEBHOOK_SECRET".into(),
    ];
    for k in keys {
        if let Ok(v) = std::env::var(&k) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn parse_http_headers(headers: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for line in headers.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            m.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    m
}

fn hmac_sha256_hex_eq(key: &[u8], msg: &[u8], presented: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return false;
    };
    mac.update(msg);
    let expect = mac.finalize().into_bytes();
    let expect_hex = hex::encode(expect);
    let presented = presented
        .trim()
        .strip_prefix("sha256=")
        .or_else(|| presented.trim().strip_prefix("v0="))
        .unwrap_or(presented.trim());
    // Constant-time-ish compare
    if expect_hex.len() != presented.len() {
        return false;
    }
    expect_hex
        .bytes()
        .zip(presented.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Parse WhatsApp cloud API verify GET query; returns challenge if token matches.
pub fn whatsapp_verify_challenge(request_target: &str) -> Option<String> {
    let q = request_target.split('?').nth(1)?;
    let mut mode = None;
    let mut token = None;
    let mut challenge = None;
    for part in q.split('&') {
        let mut kv = part.splitn(2, '=');
        let k = kv.next()?;
        let v = kv.next().unwrap_or("");
        let v = urlencoding_decode(v);
        match k {
            "hub.mode" => mode = Some(v),
            "hub.verify_token" => token = Some(v),
            "hub.challenge" => challenge = Some(v),
            _ => {}
        }
    }
    if mode.as_deref() != Some("subscribe") {
        return None;
    }
    let expected = std::env::var("WHATSAPP_VERIFY_TOKEN")
        .or_else(|_| std::env::var("PIRS_WHATSAPP_VERIFY_TOKEN"))
        .ok()?;
    if token.as_deref() == Some(expected.as_str()) {
        challenge
    } else {
        None
    }
}

fn urlencoding_decode(s: &str) -> String {
    // Minimal: + → space, %XX
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v as char);
                    i += 3;
                } else {
                    out.push('%');
                    i += 1;
                }
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

pub(super) fn extract_discord(v: &serde_json::Value) -> Option<(String, String)> {
    // Minimal: { "author_id": "...", "content": "..." } or Discord interaction-like
    let peer = v
        .get("author_id")
        .or_else(|| v.pointer("/author/id"))
        .or_else(|| v.get("user_id"))
        .and_then(|x| {
            x.as_str()
                .map(|s| s.to_string())
                .or_else(|| x.as_i64().map(|n| n.to_string()))
        })?;
    let text = v
        .get("content")
        .or_else(|| v.get("text"))
        .and_then(|x| x.as_str())?
        .to_string();
    if text.is_empty() {
        return None;
    }
    Some((peer, text))
}

pub(super) fn extract_slack(v: &serde_json::Value) -> Option<(String, String)> {
    let event = v.get("event").unwrap_or(v);
    if event.get("bot_id").is_some() {
        return None;
    }
    let peer = event
        .get("user")
        .and_then(|x| x.as_str())
        .or_else(|| event.get("channel").and_then(|x| x.as_str()))?
        .to_string();
    let text = event.get("text").and_then(|x| x.as_str())?.to_string();
    if text.is_empty() {
        return None;
    }
    Some((peer, text))
}

pub(super) fn extract_whatsapp(v: &serde_json::Value) -> Option<(String, String)> {
    // Meta Cloud API simplified: entry[0].changes[0].value.messages[0]
    let msg = v
        .pointer("/entry/0/changes/0/value/messages/0")
        .or_else(|| v.get("messages").and_then(|m| m.get(0)))?;
    let peer = msg.get("from").and_then(|x| x.as_str())?.to_string();
    let text = msg
        .pointer("/text/body")
        .and_then(|x| x.as_str())
        .or_else(|| msg.get("body").and_then(|x| x.as_str()))?
        .to_string();
    Some((peer, text))
}

pub(super) async fn run_discord_webhook_mode(
    allowlist: &PairingAllowlist,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    run_webhook_listener(
        CHANNEL_DISCORD,
        "PIRS_CLAW_DISCORD_PORT",
        8741,
        allowlist,
        extract_discord,
        |peer, text| {
            let p = peer.to_string();
            let t = text.to_string();
            Box::pin(async move { send_discord(&p, &t).await })
        },
        on_message,
    )
    .await
}

pub(super) async fn run_slack_webhook_mode(
    allowlist: &PairingAllowlist,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    run_webhook_listener(
        CHANNEL_SLACK,
        "PIRS_CLAW_SLACK_PORT",
        8742,
        allowlist,
        extract_slack,
        |peer, text| {
            let p = peer.to_string();
            let t = text.to_string();
            Box::pin(async move { send_slack(&p, &t).await })
        },
        on_message,
    )
    .await
}

pub(super) async fn run_whatsapp_webhook_mode(
    allowlist: &PairingAllowlist,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    run_webhook_listener(
        CHANNEL_WHATSAPP,
        "PIRS_CLAW_WHATSAPP_PORT",
        8743,
        allowlist,
        extract_whatsapp,
        |peer, text| {
            let p = peer.to_string();
            let t = text.to_string();
            Box::pin(async move { send_whatsapp(&p, &t).await })
        },
        on_message,
    )
    .await
}
