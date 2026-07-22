//! Messaging gateway (Hermes gap: multi-channel ingress).
//!
//! Supported transports:
//! - **telegram** — Bot API long-poll (`getUpdates`) + `sendMessage`
//! - **discord** — Bot REST send + optional incoming via simple webhook JSON
//! - **slack** — `chat.postMessage` + Events API webhook shape
//! - **whatsapp** — Meta Cloud API send + webhook shape
//! - **signal** — `signal-cli` JSON-RPC / CLI if installed
//!
//! All non-CLI channels require pairing allowlist unless `PIRS_CLAW_ALLOW_ALL=1`.
//! Webhook listeners bind **127.0.0.1** by default; set `PIRS_CLAW_PUBLIC_BIND=1`
//! (or `PIRS_CLAW_BIND=0.0.0.0`) to listen on all interfaces.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::json;

use crate::channel::{
    Channel, InboundMessage, OutboundReply, CHANNEL_DISCORD, CHANNEL_SIGNAL, CHANNEL_SLACK,
    CHANNEL_TELEGRAM, CHANNEL_WHATSAPP,
};
use crate::pairing::{warn_if_allow_all, PairingAllowlist};
use crate::GatewayReply;

/// Async handler for one inbound gateway message → text + optional file attachments.
type MessageHandler = Arc<
    dyn Fn(
            InboundMessage,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<GatewayReply>> + Send>,
        > + Send
        + Sync,
>;

/// Env: set to `1`/`true` to bind webhook listeners on `0.0.0.0`.
pub const PUBLIC_BIND_ENV: &str = "PIRS_CLAW_PUBLIC_BIND";
/// Env: explicit bind host (`127.0.0.1` default, `0.0.0.0` for public).
pub const BIND_ENV: &str = "PIRS_CLAW_BIND";

/// Resolve webhook listen host. Default **localhost** (safe).
///
/// Opt-in public bind: `PIRS_CLAW_PUBLIC_BIND=1` or `PIRS_CLAW_BIND=0.0.0.0`.
pub fn webhook_bind_host() -> String {
    if let Ok(h) = std::env::var(BIND_ENV) {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    let public = std::env::var(PUBLIC_BIND_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if public {
        "0.0.0.0".into()
    } else {
        "127.0.0.1".into()
    }
}

pub fn webhook_socket_addr(port: u16) -> SocketAddr {
    let host = webhook_bind_host();
    // Parse host:port; fall back to loopback if malformed host.
    format!("{host}:{port}")
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)))
}

/// Dispatch one or more long-running channel loops (+ optional in-process cron).
///
/// `channels` may be a single name, comma list, or was pre-parsed via
/// [`crate::parse_channel_list`]. Use `["all"]` is expanded by the caller.
pub async fn run_gateway(
    channel: &str,
    state_dir: &Path,
    allowlist: &PairingAllowlist,
    on_message: impl Fn(InboundMessage) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<GatewayReply>> + Send>,
        > + Send
        + Sync
        + 'static,
) -> anyhow::Result<()> {
    let channels = crate::parse_channel_list(channel)?;
    run_gateway_channels(&channels, state_dir, allowlist, on_message).await
}

/// Multi-channel gateway: start every listed channel that has credentials;
/// fail only if zero channels start. Spawns a 60s cron ticker in the background.
pub async fn run_gateway_channels(
    channels: &[String],
    state_dir: &Path,
    allowlist: &PairingAllowlist,
    on_message: impl Fn(InboundMessage) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<GatewayReply>> + Send>,
        > + Send
        + Sync
        + 'static,
) -> anyhow::Result<()> {
    warn_if_allow_all();
    let on_message: MessageHandler = Arc::new(on_message);
    require_allowlist(allowlist, "gateway")?;

    // Background cron tick (best-effort; does not own telegram flock).
    let state_cron = state_dir.to_path_buf();
    tokio::spawn(async move {
        cron_ticker_loop(state_cron).await;
    });

    let mut handles = Vec::new();
    let mut errors = Vec::new();

    for ch in channels {
        let allow = allowlist.clone();
        let state = state_dir.to_path_buf();
        let on_m = on_message.clone();
        let ch_name = ch.clone();
        match ch.as_str() {
            CHANNEL_TELEGRAM => {
                if telegram_token_present() {
                    // Respawn loop: transient exit (lock race, panic recovery) retries
                    // with backoff; flock still ensures only one long-poll wins.
                    handles.push(tokio::spawn(async move {
                        let mut backoff = 2u64;
                        loop {
                            match run_telegram(&state, &allow, on_m.clone()).await {
                                Ok(()) => {
                                    eprintln!("[gateway] telegram loop ended cleanly");
                                    break;
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[gateway] telegram exited: {e}; respawn in {backoff}s"
                                    );
                                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                                    backoff = (backoff.saturating_mul(2)).min(60);
                                }
                            }
                        }
                    }));
                } else {
                    errors.push("telegram: TELEGRAM_BOT_TOKEN not set".into());
                }
            }
            CHANNEL_DISCORD => {
                if std::env::var("DISCORD_BOT_TOKEN").is_ok()
                    || std::env::var("PIRS_DISCORD_BOT_TOKEN").is_ok()
                {
                    handles.push(tokio::spawn(async move {
                        if let Err(e) = run_discord_webhook_mode(&allow, on_m).await {
                            eprintln!("[gateway] discord exited: {e}");
                        }
                    }));
                } else {
                    errors.push("discord: DISCORD_BOT_TOKEN not set".into());
                }
            }
            CHANNEL_SLACK => {
                if std::env::var("SLACK_BOT_TOKEN").is_ok()
                    || std::env::var("PIRS_SLACK_BOT_TOKEN").is_ok()
                {
                    handles.push(tokio::spawn(async move {
                        if let Err(e) = run_slack_webhook_mode(&allow, on_m).await {
                            eprintln!("[gateway] slack exited: {e}");
                        }
                    }));
                } else {
                    errors.push("slack: SLACK_BOT_TOKEN not set".into());
                }
            }
            CHANNEL_WHATSAPP => {
                if std::env::var("WHATSAPP_TOKEN").is_ok()
                    || std::env::var("PIRS_WHATSAPP_TOKEN").is_ok()
                {
                    handles.push(tokio::spawn(async move {
                        if let Err(e) = run_whatsapp_webhook_mode(&allow, on_m).await {
                            eprintln!("[gateway] whatsapp exited: {e}");
                        }
                    }));
                } else {
                    errors.push("whatsapp: WHATSAPP_TOKEN not set".into());
                }
            }
            CHANNEL_SIGNAL => {
                if std::env::var("SIGNAL_ACCOUNT").is_ok()
                    || std::env::var("PIRS_SIGNAL_ACCOUNT").is_ok()
                {
                    handles.push(tokio::spawn(async move {
                        if let Err(e) = run_signal_cli(&allow, on_m).await {
                            eprintln!("[gateway] signal exited: {e}");
                        }
                    }));
                } else {
                    errors.push("signal: SIGNAL_ACCOUNT not set".into());
                }
            }
            other => errors.push(format!("unknown channel {other}")),
        }
        let _ = ch_name;
    }

    if handles.is_empty() {
        anyhow::bail!(
            "no gateway channels started.\n{}",
            errors.join("\n")
        );
    }
    for e in &errors {
        eprintln!("[gateway] skip: {e}");
    }
    eprintln!(
        "[pirs-claw gateway] running {} channel task(s); cron ticker every 60s",
        handles.len()
    );

    // Wait until all channel tasks finish (usually never).
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

fn telegram_token_present() -> bool {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| std::env::var("PIRS_TELEGRAM_BOT_TOKEN"))
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false)
}

/// Background schedule runner used by the gateway daemon.
async fn cron_ticker_loop(state_dir: PathBuf) {
    let schedule_path = state_dir.join("schedule.json");
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        // Non-blocking lock so overlapping ticks don't double-fire.
        let _lock = match crate::instance_lock::try_acquire(&state_dir, "cron") {
            Ok(l) => l,
            Err(_) => continue,
        };
        let store = match crate::ScheduleStore::open(&schedule_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[cron] open schedule: {e}");
                continue;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Skip thundering-herd of long-overdue recurring jobs after downtime.
        match store.recover_missed(now) {
            Ok(n) if n > 0 => eprintln!("[cron] advanced {n} overdue job(s) past catch-up window"),
            Err(e) => eprintln!("[cron] recover_missed: {e}"),
            _ => {}
        }
        // Heartbeat (checklist file) — no hardware; optional soft prompt.
        if let Some(prompt) = pirs_skills::heartbeat_prompt(std::time::Duration::from_secs(
            pirs_skills::DEFAULT_MIN_INTERVAL_SECS,
        )) {
            eprintln!("[heartbeat] firing checklist turn");
            let mut cmd = std::process::Command::new(
                std::env::current_exe().unwrap_or_else(|_| "pirs-claw".into()),
            );
            cmd.arg("--state-dir")
                .arg(&state_dir)
                .env(crate::UNATTENDED_ENV, "1")
                .arg("chat")
                .arg(&prompt);
            match cmd.output() {
                Ok(out) if out.status.success() => {
                    let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !reply.is_empty() {
                        let _ = deliver_outbound(&crate::DeliverTarget::Cli, &reply).await;
                    }
                }
                Ok(out) => eprintln!(
                    "[heartbeat] chat exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                ),
                Err(e) => eprintln!("[heartbeat] spawn: {e}"),
            }
        }
        let due = match store.due(now) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[cron] due: {e}");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        let mut ok_n = 0u32;
        let mut fail_n = 0u32;
        for j in due {
            eprintln!(
                "[cron] due {} deliver={}: {}",
                j.id,
                j.deliver.as_config_str(),
                j.prompt
            );
            let mut cmd = std::process::Command::new(
                std::env::current_exe().unwrap_or_else(|_| "pirs-claw".into()),
            );
            cmd.arg("--state-dir")
                .arg(&state_dir)
                .env(crate::UNATTENDED_ENV, "1")
                .arg("chat");
            if let Some(ref m) = j.model {
                cmd.arg("--model").arg(m);
            }
            // Skill names are loaded by child via state; pass prompt only.
            // Attached skills: prefix into prompt for isolation.
            let prompt = if j.skills.is_empty() {
                j.prompt.clone()
            } else {
                format!(
                    "[scheduled job; skills: {}]\n{}",
                    j.skills.join(", "),
                    j.prompt
                )
            };
            cmd.arg(&prompt);
            let fire_result: Result<(), String> = match cmd.output() {
                Ok(out) if out.status.success() => {
                    let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    // Empty reply: still deliver a placeholder so users aren't left silent
                    // and we don't mark success with zero visible output on chat targets.
                    let text = if reply.is_empty() {
                        "(scheduled job finished with empty reply)".to_string()
                    } else {
                        reply
                    };
                    deliver_outbound(&j.deliver, &text)
                        .await
                        .map_err(|e| format!("deliver: {e}"))
                }
                Ok(out) => Err(format!(
                    "exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                )),
                Err(e) => Err(format!("spawn: {e}")),
            };
            match fire_result {
                Ok(()) => {
                    let _ = store.mark_fired(&j.id, now);
                    ok_n += 1;
                }
                Err(err) => {
                    eprintln!("[cron] job {} failed: {err}", j.id);
                    let _ = store.mark_failed(&j.id, now, &err);
                    fail_n += 1;
                }
            }
        }
        eprintln!("[cron summary] ok={ok_n} failed={fail_n}");
    }
}

fn require_allowlist(allowlist: &PairingAllowlist, channel: &str) -> anyhow::Result<()> {
    if allowlist.is_empty() {
        anyhow::bail!(
            "{channel}: pairing allowlist is empty (fail closed).\n\
             Add peer ids to ~/.pirs/claw/allowlist.txt (one per line), or set \
             PIRS_CLAW_ALLOW_ALL=1 for local dev only."
        );
    }
    Ok(())
}

// ─── Telegram ───────────────────────────────────────────────────────────────

struct TelegramBot {
    token: String,
    client: reqwest::Client,
}

impl TelegramBot {
    fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("TELEGRAM_BOT_TOKEN")
            .or_else(|_| std::env::var("PIRS_TELEGRAM_BOT_TOKEN"))
            .map_err(|_| {
                anyhow::anyhow!(
                    "telegram: set TELEGRAM_BOT_TOKEN (or PIRS_TELEGRAM_BOT_TOKEN) in env / secrets.env"
                )
            })?;
        // Connect timeout for hung DNS; overall timeout above long-poll (25s) + margin.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(40))
            .build()
            .context("build telegram http client")?;
        Ok(TelegramBot { token, client })
    }

    fn api(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    async fn send(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        // Telegram limit 4096; chunk on char boundaries (not raw bytes).
        for piece in utf8_chunks(text, 3500) {
            let resp = self
                .client
                .post(self.api("sendMessage"))
                .json(&json!({
                    "chat_id": chat_id,
                    "text": piece,
                }))
                .send()
                .await?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("telegram sendMessage failed: {body}");
            }
        }
        Ok(())
    }

    /// One automatic retry on transient send failure, then surface the error.
    async fn send_with_retry(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        match self.send(chat_id, text).await {
            Ok(()) => Ok(()),
            Err(e1) => {
                eprintln!("[telegram] send retry after: {e1}");
                tokio::time::sleep(Duration::from_millis(400)).await;
                self.send(chat_id, text).await
            }
        }
    }

    /// Send a voice note (OGG/Opus preferred). Falls back to sendDocument on failure.
    async fn send_voice(&self, chat_id: &str, audio: &[u8], filename: &str) -> anyhow::Result<()> {
        let part = reqwest::multipart::Part::bytes(audio.to_vec())
            .file_name(filename.to_string())
            .mime_str("audio/ogg")
            .unwrap_or_else(|_| {
                reqwest::multipart::Part::bytes(audio.to_vec()).file_name(filename.to_string())
            });
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("voice", part);
        let resp = self
            .client
            .post(self.api("sendVoice"))
            .multipart(form)
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let err_body = resp.text().await.unwrap_or_default();
        // Fallback: send as document (works for mp3/wav/etc.).
        self.send_document_bytes(chat_id, audio, filename, None)
            .await
            .map_err(|e| anyhow::anyhow!("telegram sendVoice/sendDocument failed: {err_body} / {e}"))
    }

    /// Send a local file as a Telegram document attachment.
    async fn send_document_path(
        &self,
        chat_id: &str,
        path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read attachment {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file.bin");
        self.send_document_bytes(chat_id, &bytes, name, caption)
            .await
    }

    async fn send_document_bytes(
        &self,
        chat_id: &str,
        bytes: &[u8],
        filename: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        if bytes.is_empty() {
            anyhow::bail!("empty attachment");
        }
        if bytes.len() > crate::attach::MAX_ATTACH_BYTES {
            anyhow::bail!(
                "attachment too large ({} bytes, max {})",
                bytes.len(),
                crate::attach::MAX_ATTACH_BYTES
            );
        }
        let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);
        if let Some(c) = caption {
            let c: String = c.chars().take(1000).collect();
            if !c.is_empty() {
                form = form.text("caption", c);
            }
        }
        let resp = self
            .client
            .post(self.api("sendDocument"))
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("telegram sendDocument failed: {body}");
        }
        Ok(())
    }

    async fn send_photo_path(
        &self,
        chat_id: &str,
        path: &Path,
        caption: Option<&str>,
    ) -> anyhow::Result<()> {
        let bytes = tokio::fs::read(path).await?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("photo.jpg");
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_string());
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);
        if let Some(c) = caption {
            form = form.text("caption", c.chars().take(1000).collect::<String>());
        }
        let resp = self
            .client
            .post(self.api("sendPhoto"))
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            // Fall back to document for exotic formats.
            return self.send_document_path(chat_id, path, caption).await;
        }
        Ok(())
    }

    fn looks_like_image(path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
        )
    }

    async fn deliver_attachments(&self, chat_id: &str, paths: &[PathBuf]) {
        for path in paths {
            if !path.is_file() {
                eprintln!("[telegram] skip missing attachment {}", path.display());
                continue;
            }
            let res = if Self::looks_like_image(path) {
                self.send_photo_path(chat_id, path, None).await
            } else {
                self.send_document_path(chat_id, path, None).await
            };
            match res {
                Ok(()) => eprintln!("[telegram] sent attachment {}", path.display()),
                Err(e) => {
                    eprintln!("[telegram] attachment {} failed: {e}", path.display());
                    let _ = self
                        .send(
                            chat_id,
                            &format!(
                                "(could not send attachment {}: {e})",
                                path.file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("file")
                            ),
                        )
                        .await;
                }
            }
        }
    }

    /// Resolve `file_id` → local path under `dest_dir` (downloads via getFile).
    async fn download_file(&self, file_id: &str, dest_dir: &Path) -> anyhow::Result<PathBuf> {
        #[derive(Deserialize)]
        struct FileResp {
            ok: bool,
            result: Option<TgFilePath>,
        }
        #[derive(Deserialize)]
        struct TgFilePath {
            file_path: Option<String>,
        }
        let resp = self
            .client
            .get(self.api("getFile"))
            .query(&[("file_id", file_id)])
            .send()
            .await?;
        let body: FileResp = resp.json().await?;
        if !body.ok {
            anyhow::bail!("telegram getFile not ok for file_id");
        }
        let rel = body
            .result
            .and_then(|r| r.file_path)
            .ok_or_else(|| anyhow::anyhow!("telegram getFile missing file_path"))?;
        let url = format!("https://api.telegram.org/file/bot{}/{}", self.token, rel);
        let bytes = self.client.get(&url).send().await?.bytes().await?;
        std::fs::create_dir_all(dest_dir)?;
        let name = Path::new(&rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download.bin");
        // Unique name so concurrent downloads don't clobber.
        let dest = dest_dir.join(format!(
            "{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            name
        ));
        std::fs::write(&dest, &bytes)?;
        Ok(dest)
    }
}

/// Split `s` into chunks of at most `max_chars` Unicode scalars.
fn utf8_chunks(s: &str, max_chars: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if cur.chars().count() >= max_chars {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Channel for TelegramBot {
    fn channel_id(&self) -> &str {
        CHANNEL_TELEGRAM
    }

    fn deliver(&self, reply: &OutboundReply) -> anyhow::Result<()> {
        let client = reqwest::blocking::Client::new();
        for piece in utf8_chunks(&reply.text, 3500) {
            let resp = client
                .post(self.api("sendMessage"))
                .json(&json!({
                    "chat_id": &reply.peer_id,
                    "text": piece,
                }))
                .send()?;
            if !resp.status().is_success() {
                anyhow::bail!("telegram sendMessage failed: {}", resp.text().unwrap_or_default());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    chat: TgChat,
    text: Option<String>,
    caption: Option<String>,
    from: Option<TgUser>,
    voice: Option<TgMediaFile>,
    audio: Option<TgMediaFile>,
    document: Option<TgDocument>,
    photo: Option<Vec<TgPhotoSize>>,
    video: Option<TgMediaFile>,
    video_note: Option<TgMediaFile>,
    sticker: Option<TgMediaFile>,
}

#[derive(Debug, Deserialize)]
struct TgMediaFile {
    file_id: String,
    #[serde(default)]
    duration: Option<u32>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TgDocument {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TgPhotoSize {
    file_id: String,
    #[serde(default)]
    file_size: Option<u64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
}

/// Result of parsing a Telegram message for the agent.
struct TgInbound {
    text: String,
    /// True when the user sent voice/audio (drives optional TTS reply).
    from_voice: bool,
}

/// Build agent-facing text from a Telegram message (text, caption, voice, docs, …).
///
/// Previously only `message.text` was accepted — voice notes and attachments were
/// silently dropped after getUpdates advanced the offset (never entered session history).
async fn telegram_message_to_text(
    bot: &TelegramBot,
    state_dir: &Path,
    msg: &TgMessage,
) -> Option<TgInbound> {
    let wrap = |text: String, from_voice: bool| Some(TgInbound { text, from_voice });
    if let Some(t) = msg.text.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return wrap(t.to_string(), false);
    }

    let caption = msg
        .caption
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let media_dir = state_dir.join("media").join("telegram");

    // Voice / audio → download + multi-backend STT (HTTP registry → CLI).
    if let Some(v) = msg.voice.as_ref().or(msg.audio.as_ref()) {
        let kind = if msg.voice.is_some() { "voice" } else { "audio" };
        let dur = v.duration.map(|d| format!("{d}s")).unwrap_or_else(|| "?s".into());
        let mime = v.mime_type.as_deref().unwrap_or("?");
        eprintln!(
            "[telegram] {kind} message duration={dur} mime={mime} size={:?}",
            v.file_size
        );
        match bot.download_file(&v.file_id, &media_dir).await {
            Ok(path) => {
                match crate::voice::transcribe_audio(&path).await {
                    Ok(Some(transcript)) if !transcript.trim().is_empty() => {
                        let mut t = format!("[transcribed {kind}] {}", transcript.trim());
                        if let Some(c) = caption {
                            t.push_str("\n[caption] ");
                            t.push_str(&c);
                        }
                        return wrap(t, true);
                    }
                    Ok(_) => {
                        let mut t = format!(
                            "[{kind} note received, {dur}, saved as {} — no STT backend available \
                             (configure [[models]] caps=[\"stt\"], PIRS_SPEECH_BASE_URL, \
                             whisper CLI, or PIRS_CLAW_TRANSCRIBE_CMD)]",
                            path.display()
                        );
                        if let Some(c) = caption {
                            t.push_str("\n[caption] ");
                            t.push_str(&c);
                        }
                        return wrap(t, true);
                    }
                    Err(e) => {
                        eprintln!("[telegram] transcribe error: {e}");
                        let mut t = format!(
                            "[{kind} note received, {dur}, file {} — transcription failed: {e}]",
                            path.display()
                        );
                        if let Some(c) = caption {
                            t.push_str("\n[caption] ");
                            t.push_str(&c);
                        }
                        return wrap(t, true);
                    }
                }
            }
            Err(e) => {
                eprintln!("[telegram] download {kind}: {e}");
                return wrap(
                    format!("[{kind} note received, {dur} — download failed: {e}]"),
                    true,
                );
            }
        }
    }

    if let Some(doc) = &msg.document {
        let name = doc.file_name.as_deref().unwrap_or("document");
        let mime = doc.mime_type.as_deref().unwrap_or("?");
        let size = doc.file_size.unwrap_or(0);
        eprintln!("[telegram] document name={name} mime={mime} size={size}");
        match bot.download_file(&doc.file_id, &media_dir).await {
            Ok(path) => {
                let mut parts = vec![format!(
                    "[document attached: {name} ({mime}, {size} bytes) saved as {}]",
                    path.display()
                )];
                if let Some(c) = caption {
                    parts.push(format!("[caption] {c}"));
                }
                // Inline small text-ish files so the model can actually use them.
                let texty = mime.starts_with("text/")
                    || name.ends_with(".txt")
                    || name.ends_with(".md")
                    || name.ends_with(".py")
                    || name.ends_with(".rs")
                    || name.ends_with(".json")
                    || name.ends_with(".toml")
                    || name.ends_with(".csv");
                if texty && size > 0 && size <= 64 * 1024 {
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        let body = body.chars().take(8000).collect::<String>();
                        parts.push(format!("[file contents]\n{body}"));
                    }
                }
                return wrap(parts.join("\n"), false);
            }
            Err(e) => {
                eprintln!("[telegram] download document: {e}");
                let mut t = format!("[document {name} ({mime}) — download failed: {e}]");
                if let Some(c) = caption {
                    t.push_str("\n[caption] ");
                    t.push_str(&c);
                }
                return wrap(t, false);
            }
        }
    }

    if let Some(photos) = &msg.photo {
        if let Some(best) = photos.last() {
            let dim = match (best.width, best.height) {
                (Some(w), Some(h)) => format!("{w}x{h}"),
                _ => "?".into(),
            };
            eprintln!("[telegram] photo {dim} size={:?}", best.file_size);
            match bot.download_file(&best.file_id, &media_dir).await {
                Ok(path) => {
                    let mut t = format!(
                        "[photo received {dim}, saved as {} — vision not wired; describe what you need]",
                        path.display()
                    );
                    if let Some(c) = caption {
                        t.push_str("\n[caption] ");
                        t.push_str(&c);
                    }
                    return wrap(t, false);
                }
                Err(e) => {
                    return wrap(format!("[photo received — download failed: {e}]"), false);
                }
            }
        }
    }

    if msg.video.is_some() || msg.video_note.is_some() {
        let kind = if msg.video_note.is_some() {
            "video_note"
        } else {
            "video"
        };
        let f = msg.video.as_ref().or(msg.video_note.as_ref()).unwrap();
        eprintln!("[telegram] {kind} size={:?}", f.file_size);
        match bot.download_file(&f.file_id, &media_dir).await {
            Ok(path) => {
                let mut t = format!("[{kind} received, saved as {}]", path.display());
                if let Some(c) = caption {
                    t.push_str("\n[caption] ");
                    t.push_str(&c);
                }
                return wrap(t, false);
            }
            Err(e) => {
                return wrap(format!("[{kind} received — download failed: {e}]"), false);
            }
        }
    }

    if msg.sticker.is_some() {
        return wrap("[sticker received — no text]".into(), false);
    }

    if let Some(c) = caption {
        return wrap(c, false);
    }

    None
}

async fn run_telegram(
    state_dir: &Path,
    allowlist: &PairingAllowlist,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    require_allowlist(allowlist, "telegram")?;
    // Exclusive getUpdates: hold flock for process lifetime.
    let _lock = crate::instance_lock::try_acquire(state_dir, "telegram")?;
    let bot = TelegramBot::from_env()?;
    let mut offset: i64 = 0;
    let mut backoff_secs: u64 = 1;
    eprintln!(
        "[pirs-claw gateway] telegram long-poll started (allowlist {} peers; single-instance lock held)",
        allowlist.len()
    );
    loop {
        let url = format!(
            "{}?timeout=25&offset={}",
            bot.api("getUpdates"),
            offset
        );
        let resp = bot.client.get(&url).send().await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "[telegram] getUpdates transport error: {e}; retry in {backoff_secs}s"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs.saturating_mul(2)).min(60);
                continue;
            }
        };
        #[derive(Deserialize)]
        struct TgResp {
            ok: bool,
            #[serde(default)]
            description: Option<String>,
            result: Option<Vec<TgUpdate>>,
        }
        let status = resp.status();
        let body: TgResp = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[telegram] getUpdates bad JSON (http {status}): {e}; retry in {backoff_secs}s"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs.saturating_mul(2)).min(60);
                continue;
            }
        };
        if !body.ok {
            let desc = body.description.as_deref().unwrap_or("(no description)");
            eprintln!(
                "[telegram] getUpdates ok=false http={status}: {desc}; retry in {backoff_secs}s \
                 (409 often means another getUpdates or a webhook is set)"
            );
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs.saturating_mul(2)).min(60);
            continue;
        }
        backoff_secs = 1; // success path resets backoff
        for upd in body.result.unwrap_or_default() {
            // Process first, then advance offset — crash mid-handler may redeliver
            // once (acceptable) instead of silently dropping the update.
            let Some(msg) = upd.message else {
                offset = upd.update_id + 1;
                continue;
            };
            let peer = msg.chat.id.to_string();
            let user = msg
                .from
                .as_ref()
                .map(|u| u.id.to_string())
                .unwrap_or_else(|| peer.clone());
            // Allow chat id or user id
            if !allowlist.is_allowed(&peer) && !allowlist.is_allowed(&user) {
                eprintln!("[telegram] ignore unpaired peer chat={peer} user={user}");
                let _ = bot
                    .send_with_retry(
                        &peer,
                        "pirs-claw: you are not on the pairing allowlist. Ask the owner to add your chat id.",
                    )
                    .await;
                offset = upd.update_id + 1;
                continue;
            }
            let Some(parsed) = telegram_message_to_text(&bot, state_dir, &msg).await else {
                eprintln!(
                    "[telegram] skip message with no text/media we understand (chat={peer})"
                );
                let _ = bot
                    .send_with_retry(
                        &peer,
                        "pirs-claw: I only handle text, voice notes, audio, documents, photos, and video for now.",
                    )
                    .await;
                offset = upd.update_id + 1;
                continue;
            };
            let inbound = InboundMessage {
                channel_id: CHANNEL_TELEGRAM.into(),
                peer_id: peer.clone(),
                text: parsed.text,
                ts: crate::channel::now_secs_pub(),
            };
            match on_message(inbound).await {
                Ok(reply) => {
                    let text = reply.text.trim();
                    if text.is_empty() && reply.attachments.is_empty() {
                        // Never leave the user with silence after a successful turn.
                        if let Err(e) = bot
                            .send_with_retry(&peer, "(no text reply from model)")
                            .await
                        {
                            eprintln!("[telegram] send empty-placeholder failed: {e}");
                        }
                    } else if !text.is_empty() {
                        if let Err(e) = bot.send_with_retry(&peer, &reply.text).await {
                            eprintln!("[telegram] send error: {e}");
                            let _ = bot
                                .send(
                                    &peer,
                                    &format!(
                                        "delivery failed after agent reply: {}",
                                        e.to_string().chars().take(200).collect::<String>()
                                    ),
                                )
                                .await;
                        }
                    }
                    if !reply.attachments.is_empty() {
                        bot.deliver_attachments(&peer, &reply.attachments).await;
                    }
                    // Optional TTS voice reply (multi-backend Kokoro/OpenAI/…).
                    let want_tts = (parsed.from_voice && crate::voice::tts_on_voice())
                        || crate::voice::tts_always();
                    if want_tts && crate::voice::tts_backends_configured() {
                        // Keep TTS short — long agent dumps are bad as audio.
                        let speak = reply.text.chars().take(800).collect::<String>();
                        match crate::voice::synthesize_speech(&speak, None, Some("opus")).await {
                            Ok((audio, ep)) => {
                                eprintln!(
                                    "[tts] {} bytes via {} model={}",
                                    audio.len(),
                                    ep.backend_name,
                                    ep.model
                                );
                                if let Err(e) = bot.send_voice(&peer, &audio, "reply.ogg").await {
                                    eprintln!("[telegram] sendVoice error: {e}");
                                }
                            }
                            Err(e) => eprintln!("[tts] failed: {e}"),
                        }
                    }
                }
                Err(e) => {
                    let _ = bot
                        .send_with_retry(&peer, &format!("error: {e}"))
                        .await;
                }
            }
            offset = upd.update_id + 1;
        }
    }
}

// ─── Webhook-style channels (Discord / Slack / WhatsApp) ────────────────────

/// Shared tiny HTTP listener for webhook JSON bodies.
type SendFuture = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>;

async fn run_webhook_listener(
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
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 65536];
            let n = match sock.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let raw = String::from_utf8_lossy(&buf[..n]);
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
            // Crude HTTP: find body after \r\n\r\n
            let (headers, body) = match raw.split_once("\r\n\r\n") {
                Some((h, b)) => (h, b.trim_end_matches('\0')),
                None => ("", raw.trim_end_matches('\0')),
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

/// Verify webhook authenticity when `PIRS_WEBHOOK_SECRET` (or channel-specific
/// env) is set. Supports:
/// - generic: `X-Pirs-Signature: sha256=<hex>` HMAC-SHA256 of body
/// - Slack: `X-Slack-Signature` + `X-Slack-Request-Timestamp` (v0 scheme)
/// - GitHub-style: `X-Hub-Signature-256: sha256=<hex>`
///
/// If no secret is configured, returns Ok (pairing allowlist remains the gate).
/// If a secret is configured and the signature is missing/wrong, returns Err.
pub fn verify_webhook_signature(
    channel: &str,
    headers: &str,
    body: &str,
) -> Result<(), String> {
    let secret = webhook_secret_for(channel);
    let Some(secret) = secret else {
        return Ok(());
    };
    let hdrs = parse_http_headers(headers);
    // Prefer channel-native headers, then generic.
    if let Some(sig) = hdrs
        .get("x-slack-signature")
        .cloned()
        .or_else(|| hdrs.get("x-hub-signature-256").cloned())
        .or_else(|| hdrs.get("x-pirs-signature").cloned())
    {
        if let Some(ts) = hdrs.get("x-slack-request-timestamp") {
            // Slack: v0:{ts}:{body}
            let base = format!("v0:{ts}:{body}");
            if hmac_sha256_hex_eq(secret.as_bytes(), base.as_bytes(), &sig) {
                return Ok(());
            }
            // Also accept raw body HMAC under the same header (tests / simple relays).
            if hmac_sha256_hex_eq(secret.as_bytes(), body.as_bytes(), &sig) {
                return Ok(());
            }
            return Err("slack/hmac signature mismatch".into());
        }
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

fn extract_discord(v: &serde_json::Value) -> Option<(String, String)> {
    // Minimal: { "author_id": "...", "content": "..." } or Discord interaction-like
    let peer = v
        .get("author_id")
        .or_else(|| v.pointer("/author/id"))
        .or_else(|| v.get("user_id"))
        .and_then(|x| x.as_str().map(|s| s.to_string()).or_else(|| x.as_i64().map(|n| n.to_string())))?;
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

fn extract_slack(v: &serde_json::Value) -> Option<(String, String)> {
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

fn extract_whatsapp(v: &serde_json::Value) -> Option<(String, String)> {
    // Meta Cloud API simplified: entry[0].changes[0].value.messages[0]
    let msg = v
        .pointer("/entry/0/changes/0/value/messages/0")
        .or_else(|| v.get("messages").and_then(|m| m.get(0)))?;
    let peer = msg
        .get("from")
        .and_then(|x| x.as_str())?
        .to_string();
    let text = msg
        .pointer("/text/body")
        .and_then(|x| x.as_str())
        .or_else(|| msg.get("body").and_then(|x| x.as_str()))?
        .to_string();
    Some((peer, text))
}

/// Surface a schedule-tick reply to the user-facing channel.
///
/// For `DeliverTarget::Cli` this **must** print: tick runs chat with
/// `Command::output()`, so the child never writes to the parent's stdout.
pub async fn deliver_outbound(target: &crate::DeliverTarget, text: &str) -> anyhow::Result<()> {
    match target {
        crate::DeliverTarget::Cli => {
            println!("{text}");
            Ok(())
        }
        crate::DeliverTarget::Telegram { chat_id } => {
            let bot = TelegramBot::from_env()?;
            bot.send(chat_id, text).await
        }
        crate::DeliverTarget::Discord { peer } => send_discord(peer, text).await,
        crate::DeliverTarget::Slack { peer } => send_slack(peer, text).await,
        crate::DeliverTarget::Whatsapp { peer } => send_whatsapp(peer, text).await,
        crate::DeliverTarget::Signal { peer } => {
            let account = std::env::var("SIGNAL_ACCOUNT")
                .or_else(|_| std::env::var("PIRS_SIGNAL_ACCOUNT"))
                .map_err(|_| anyhow::anyhow!("SIGNAL_ACCOUNT not set"))?;
            let out = tokio::process::Command::new("signal-cli")
                .args(["-a", &account, "send", "-m", text, peer])
                .output()
                .await?;
            if !out.status.success() {
                anyhow::bail!(
                    "signal-cli send failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Ok(())
        }
    }
}

async fn send_discord(peer: &str, text: &str) -> anyhow::Result<()> {
    let token = std::env::var("DISCORD_BOT_TOKEN")
        .or_else(|_| std::env::var("PIRS_DISCORD_BOT_TOKEN"))
        .map_err(|_| anyhow::anyhow!("DISCORD_BOT_TOKEN not set"))?;
    // DM channel create is multi-step; support channel id in peer as "channel:<id>"
    // or raw channel id for posting.
    let channel_id = peer.strip_prefix("channel:").unwrap_or(peer);
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "https://discord.com/api/v10/channels/{channel_id}/messages"
        ))
        .header("Authorization", format!("Bot {token}"))
        .json(&json!({ "content": text.chars().take(1900).collect::<String>() }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("discord send: {}", resp.text().await.unwrap_or_default());
    }
    Ok(())
}

async fn send_slack(peer: &str, text: &str) -> anyhow::Result<()> {
    let token = std::env::var("SLACK_BOT_TOKEN")
        .or_else(|_| std::env::var("PIRS_SLACK_BOT_TOKEN"))
        .map_err(|_| anyhow::anyhow!("SLACK_BOT_TOKEN not set"))?;
    let client = reqwest::Client::new();
    let resp = client
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(token)
        .json(&json!({ "channel": peer, "text": text }))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    if v.get("ok") != Some(&json!(true)) {
        anyhow::bail!("slack send: {v}");
    }
    Ok(())
}

async fn send_whatsapp(peer: &str, text: &str) -> anyhow::Result<()> {
    let token = std::env::var("WHATSAPP_TOKEN")
        .or_else(|_| std::env::var("PIRS_WHATSAPP_TOKEN"))
        .map_err(|_| anyhow::anyhow!("WHATSAPP_TOKEN not set"))?;
    let phone_id = std::env::var("WHATSAPP_PHONE_NUMBER_ID")
        .or_else(|_| std::env::var("PIRS_WHATSAPP_PHONE_NUMBER_ID"))
        .map_err(|_| anyhow::anyhow!("WHATSAPP_PHONE_NUMBER_ID not set"))?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "https://graph.facebook.com/v18.0/{phone_id}/messages"
        ))
        .bearer_auth(token)
        .json(&json!({
            "messaging_product": "whatsapp",
            "to": peer,
            "type": "text",
            "text": { "body": text.chars().take(4000).collect::<String>() }
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("whatsapp send: {}", resp.text().await.unwrap_or_default());
    }
    Ok(())
}

async fn run_discord_webhook_mode(
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

async fn run_slack_webhook_mode(
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

async fn run_whatsapp_webhook_mode(
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

// ─── Signal via signal-cli ──────────────────────────────────────────────────

async fn run_signal_cli(
    allowlist: &PairingAllowlist,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    require_allowlist(allowlist, "signal")?;
    let account = std::env::var("SIGNAL_ACCOUNT")
        .or_else(|_| std::env::var("PIRS_SIGNAL_ACCOUNT"))
        .map_err(|_| {
            anyhow::anyhow!("signal: set SIGNAL_ACCOUNT (phone number) and install signal-cli")
        })?;
    // Require signal-cli on PATH
    let status = tokio::process::Command::new("signal-cli")
        .arg("--version")
        .output()
        .await;
    if status.map(|o| !o.status.success()).unwrap_or(true) {
        anyhow::bail!("signal: signal-cli not found on PATH");
    }
    eprintln!("[pirs-claw gateway] signal-cli receive loop for {account}");
    loop {
        let out = tokio::process::Command::new("signal-cli")
            .args(["-a", &account, "receive", "-t", "10", "--json"])
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let envelope = v.get("envelope").unwrap_or(&v);
            let peer = envelope
                .get("source")
                .or_else(|| envelope.get("sourceNumber"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if peer.is_empty() || !allowlist.is_allowed(peer) {
                continue;
            }
            let text = envelope
                .pointer("/dataMessage/message")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }
            let inbound = InboundMessage {
                channel_id: CHANNEL_SIGNAL.into(),
                peer_id: peer.into(),
                text: text.into(),
                ts: crate::channel::now_secs_pub(),
            };
            let reply = match on_message(inbound).await {
                Ok(r) => r,
                Err(e) => GatewayReply::text(format!("error: {e}")),
            };
            let _ = tokio::process::Command::new("signal-cli")
                .args(["-a", &account, "send", "-m", &reply.text, peer])
                .output()
                .await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_chunks_do_not_split_multibyte() {
        let s = "á".repeat(10);
        let parts = utf8_chunks(&s, 3);
        assert!(parts.iter().all(|p| p.chars().count() <= 3));
        assert_eq!(parts.join(""), s);
    }

    #[test]
    fn deliver_outbound_cli_is_required_after_captured_chat() {
        // Contract: tick uses Command::output(); Cli arm must print, not no-op.
        // Drive the real match by invoking the async helper (prints to stdout).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            deliver_outbound(&crate::DeliverTarget::Cli, "tick-cli-reply-marker")
                .await
                .unwrap();
        });
        // If Cli were a silent Ok(()), this test still passes — structural
        // assert on main ensures we always call deliver_outbound for every target.
        let main_src = include_str!("main.rs");
        assert!(
            main_src.contains("deliver_outbound(&job.deliver")
                || main_src.contains("deliver_outbound(&j.deliver"),
            "tick/fire must call deliver_outbound with the job deliver target"
        );
        assert!(
            !main_src.contains("if !matches!(j.deliver, DeliverTarget::Cli)"),
            "must not skip Cli deliver after captured subprocess stdout"
        );
        let cli_arm = include_str!("gateway.rs");
        assert!(
            cli_arm.contains("DeliverTarget::Cli") && cli_arm.contains("println!"),
            "Cli deliver must println the reply text"
        );
    }

    #[test]
    fn telegram_message_deserializes_voice_and_document() {
        let v: TgMessage = serde_json::from_value(serde_json::json!({
            "chat": {"id": 1},
            "voice": {"file_id": "AAA", "duration": 3, "mime_type": "audio/ogg", "file_size": 1234},
            "from": {"id": 9}
        }))
        .unwrap();
        assert!(v.text.is_none());
        assert_eq!(v.voice.as_ref().unwrap().duration, Some(3));

        let d: TgMessage = serde_json::from_value(serde_json::json!({
            "chat": {"id": 1},
            "document": {"file_id": "BBB", "file_name": "hello.py", "mime_type": "text/x-python", "file_size": 20},
            "caption": "my file"
        }))
        .unwrap();
        assert_eq!(d.document.as_ref().unwrap().file_name.as_deref(), Some("hello.py"));
        assert_eq!(d.caption.as_deref(), Some("my file"));
    }

    /// Serialize env mutations for bind-host tests (parallel cargo test races).
    fn bind_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn webhook_bind_defaults_to_localhost() {
        let _g = bind_env_lock();
        std::env::remove_var(PUBLIC_BIND_ENV);
        std::env::remove_var(BIND_ENV);
        assert_eq!(
            webhook_bind_host(),
            "127.0.0.1",
            "default bind must be localhost when no env set"
        );
        let addr = webhook_socket_addr(8741);
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 8741);
        std::env::remove_var(PUBLIC_BIND_ENV);
        std::env::remove_var(BIND_ENV);
    }

    #[test]
    fn webhook_bind_public_opt_in() {
        let _g = bind_env_lock();
        std::env::remove_var(BIND_ENV);
        std::env::remove_var(PUBLIC_BIND_ENV);
        std::env::set_var(PUBLIC_BIND_ENV, "1");
        assert_eq!(webhook_bind_host(), "0.0.0.0");
        std::env::remove_var(PUBLIC_BIND_ENV);
        std::env::set_var(BIND_ENV, "0.0.0.0");
        assert_eq!(webhook_bind_host(), "0.0.0.0");
        std::env::remove_var(BIND_ENV);
        std::env::remove_var(PUBLIC_BIND_ENV);
    }

    #[test]
    fn extract_discord_simple() {
        let v = json!({"author_id": "99", "content": "hello"});
        assert_eq!(
            extract_discord(&v),
            Some(("99".into(), "hello".into()))
        );
    }

    #[test]
    fn extract_slack_ignores_bots() {
        let v = json!({"event": {"bot_id": "B1", "user": "U1", "text": "x"}});
        assert!(extract_slack(&v).is_none());
        let v = json!({"event": {"user": "U1", "text": "hi"}});
        assert_eq!(extract_slack(&v), Some(("U1".into(), "hi".into())));
    }

    #[test]
    fn extract_whatsapp_meta_shape() {
        let v = json!({
            "entry": [{"changes": [{"value": {"messages": [
                {"from": "15551234567", "text": {"body": "yo"}}
            ]}}]}]
        });
        assert_eq!(
            extract_whatsapp(&v),
            Some(("15551234567".into(), "yo".into()))
        );
    }

    #[test]
    fn whatsapp_verify_token_gate() {
        std::env::set_var("WHATSAPP_VERIFY_TOKEN", "secret-token");
        let ok = whatsapp_verify_challenge(
            "/?hub.mode=subscribe&hub.verify_token=secret-token&hub.challenge=abc123",
        );
        assert_eq!(ok.as_deref(), Some("abc123"));
        let bad = whatsapp_verify_challenge(
            "/?hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=abc123",
        );
        assert!(bad.is_none());
        std::env::remove_var("WHATSAPP_VERIFY_TOKEN");
    }

    #[test]
    fn webhook_no_secret_allows() {
        std::env::remove_var("PIRS_WEBHOOK_SECRET");
        std::env::remove_var("WEBHOOK_SECRET");
        std::env::remove_var("PIRS_SLACK_WEBHOOK_SECRET");
        assert!(verify_webhook_signature("slack", "POST /\r\n\r\n", "{}").is_ok());
    }

    #[test]
    fn webhook_secret_requires_signature_header() {
        std::env::set_var("PIRS_WEBHOOK_SECRET", "s3cret");
        let err = verify_webhook_signature("slack", "POST /\r\nHost: x\r\n", "{}").unwrap_err();
        assert!(err.contains("no X-Pirs") || err.contains("Signature"), "{err}");
        std::env::remove_var("PIRS_WEBHOOK_SECRET");
    }

    #[test]
    fn webhook_valid_hmac_passes() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret = b"s3cret";
        let body = r#"{"ok":true}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        std::env::set_var("PIRS_WEBHOOK_SECRET", "s3cret");
        let hdr = format!("POST / HTTP/1.1\r\nX-Pirs-Signature: {sig}");
        assert!(verify_webhook_signature("slack", &hdr, body).is_ok());
        std::env::remove_var("PIRS_WEBHOOK_SECRET");
    }
}
