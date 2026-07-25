//! Telegram Bot API long-poll + send.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::json;

use crate::channel::{
    Channel, InboundMessage, OutboundReply, CHANNEL_TELEGRAM,
};
use crate::pairing::PairingAllowlist;
use crate::GatewayReply;

use super::allow::require_allowlist_for_state;
use super::utf8::utf8_chunks;
use super::MessageHandler;


pub(super) fn telegram_token_present() -> bool {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| std::env::var("PIRS_TELEGRAM_BOT_TOKEN"))
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false)
}


// ─── Telegram ───────────────────────────────────────────────────────────────

pub(super) struct TelegramBot {
    token: String,
    client: reqwest::Client,
}

impl TelegramBot {
    pub(super) fn from_env() -> anyhow::Result<Self> {
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

    pub(super) async fn send(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
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
pub(super) struct TgUpdate {
    pub(super) update_id: i64,
    pub(super) message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TgMessage {
    pub(super) chat: TgChat,
    pub(super) text: Option<String>,
    pub(super) caption: Option<String>,
    pub(super) from: Option<TgUser>,
    pub(super) voice: Option<TgMediaFile>,
    pub(super) audio: Option<TgMediaFile>,
    pub(super) document: Option<TgDocument>,
    pub(super) photo: Option<Vec<TgPhotoSize>>,
    pub(super) video: Option<TgMediaFile>,
    pub(super) video_note: Option<TgMediaFile>,
    pub(super) sticker: Option<TgMediaFile>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TgMediaFile {
    pub(super) file_id: String,
    #[serde(default)]
    pub(super) duration: Option<u32>,
    #[serde(default)]
    pub(super) mime_type: Option<String>,
    #[serde(default)]
    pub(super) file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TgDocument {
    pub(super) file_id: String,
    #[serde(default)]
    pub(super) file_name: Option<String>,
    #[serde(default)]
    pub(super) mime_type: Option<String>,
    #[serde(default)]
    pub(super) file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TgPhotoSize {
    pub(super) file_id: String,
    #[serde(default)]
    pub(super) file_size: Option<u64>,
    #[serde(default)]
    pub(super) width: Option<u32>,
    #[serde(default)]
    pub(super) height: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TgChat {
    pub(super) id: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct TgUser {
    pub(super) id: i64,
}

/// Result of parsing a Telegram message for the agent.
pub(super) struct TgInbound {
    pub(super) text: String,
    /// True when the user sent voice/audio (drives optional TTS reply).
    pub(super) from_voice: bool,
}

/// Build agent-facing text from a Telegram message (text, caption, voice, docs, …).
///
/// Previously only `message.text` was accepted — voice notes and attachments were
/// silently dropped after getUpdates advanced the offset (never entered session history).
pub(super) async fn telegram_message_to_text(
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

pub(super) async fn run_telegram(
    state_dir: &Path,
    allowlist: &PairingAllowlist,
    on_message: MessageHandler,
) -> anyhow::Result<()> {
    require_allowlist_for_state(allowlist, "telegram", Some(state_dir))?;
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
            // Allow chat id or user id; or redeem a pending pairing code from text.
            let mut allowlist = allowlist.clone();
            if !allowlist.is_allowed(&peer) && !allowlist.is_allowed(&user) {
                let text_probe = msg.text.as_deref().unwrap_or("").trim().to_string();
                let allow_path = crate::pairing::PairingAllowlist::default_path(state_dir);
                let redeemed = crate::pairing::try_redeem_pairing_code(
                    state_dir,
                    &allow_path,
                    &text_probe,
                    &peer,
                )
                .unwrap_or(false);
                if redeemed {
                    // Reload allowlist after redeem.
                    if let Ok(al) = crate::pairing::PairingAllowlist::open(&allow_path) {
                        allowlist = al;
                    }
                    let _ = bot
                        .send_with_retry(
                            &peer,
                            "pirs-claw: paired successfully. You can chat now.",
                        )
                        .await;
                    offset = upd.update_id + 1;
                    continue;
                }
                eprintln!("[telegram] ignore unpaired peer chat={peer} user={user}");
                let hint = if crate::pairing::looks_like_pairing_code(&text_probe) {
                    "that pairing code is invalid or expired. Ask the owner for a new `pirs-claw pair code`."
                } else {
                    "you are not on the pairing allowlist. Ask the owner to run `pirs-claw pair add <chat_id>` or mint a code with `pirs-claw pair code`."
                };
                let _ = bot
                    .send_with_retry(&peer, &format!("pirs-claw: {hint}"))
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
