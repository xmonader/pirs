//! Messaging gateway (Hermes gap: multi-channel ingress).
//!
//! Supported transports: telegram, discord, slack, whatsapp, signal.
//! Webhook listeners bind **127.0.0.1** by default; set `PIRS_CLAW_PUBLIC_BIND=1`
//! (or `PIRS_CLAW_BIND=0.0.0.0`) to listen on all interfaces.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::channel::{
    InboundMessage, CHANNEL_DISCORD, CHANNEL_SIGNAL, CHANNEL_SLACK, CHANNEL_TELEGRAM,
    CHANNEL_WHATSAPP,
};
use crate::pairing::{warn_if_allow_all, PairingAllowlist};
use crate::GatewayReply;

mod allow;
mod bind;
mod cron;
mod outbound;
mod signal;
mod telegram;
mod utf8;
mod webhook;

pub use bind::{webhook_bind_host, webhook_socket_addr, BIND_ENV, PUBLIC_BIND_ENV};
pub use outbound::deliver_outbound;
pub use utf8::utf8_chunks;
pub use webhook::{
    read_http_request, verify_webhook_signature, verify_webhook_signature_at,
    whatsapp_verify_challenge,
};

/// Async handler for one inbound gateway message → text + optional file attachments.
pub(super) type MessageHandler = Arc<
    dyn Fn(
            InboundMessage,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<GatewayReply>> + Send>,
        > + Send
        + Sync,
>;

/// Dispatch one or more long-running channel loops (+ optional in-process cron).
///
/// `channels` may be a single name, comma list, or was pre-parsed via
/// [`crate::parse_channel_list`]. Use `["all"]` is expanded by the caller.
pub async fn run_gateway(
    channel: &str,
    state_dir: &Path,
    allowlist: &PairingAllowlist,
    on_message: impl Fn(
            InboundMessage,
        ) -> std::pin::Pin<
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
    on_message: impl Fn(
            InboundMessage,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<GatewayReply>> + Send>,
        > + Send
        + Sync
        + 'static,
) -> anyhow::Result<()> {
    warn_if_allow_all();
    let on_message: MessageHandler = Arc::new(on_message);
    allow::require_allowlist_for_state(allowlist, "gateway", Some(state_dir))?;

    // Background cron tick (best-effort; does not own telegram flock).
    let state_cron = state_dir.to_path_buf();
    tokio::spawn(async move {
        cron::cron_ticker_loop(state_cron).await;
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
                if telegram::telegram_token_present() {
                    // Respawn loop: transient exit (lock race, panic recovery) retries
                    // with backoff; flock still ensures only one long-poll wins.
                    handles.push(tokio::spawn(async move {
                        let mut backoff = 2u64;
                        loop {
                            match telegram::run_telegram(&state, &allow, on_m.clone()).await {
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
                        if let Err(e) = webhook::run_discord_webhook_mode(&allow, on_m).await {
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
                        if let Err(e) = webhook::run_slack_webhook_mode(&allow, on_m).await {
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
                        if let Err(e) = webhook::run_whatsapp_webhook_mode(&allow, on_m).await {
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
                        if let Err(e) = signal::run_signal_cli(&allow, on_m).await {
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
        anyhow::bail!("no gateway channels started.\n{}", errors.join("\n"));
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

#[cfg(test)]
mod tests {
    use super::telegram::TgMessage;
    use super::webhook::{
        extract_discord, extract_slack, extract_whatsapp, find_header_end, parse_content_length,
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn utf8_chunks_do_not_split_multibyte() {
        let s = "á".repeat(10);
        let parts = utf8_chunks(&s, 3);
        assert!(parts.iter().all(|p| p.chars().count() <= 3));
        assert_eq!(parts.join(""), s);
    }

    #[tokio::test]
    async fn deliver_outbound_telegram_fails_closed_without_token() {
        // Honest failure: no silent Ok when Telegram cannot send.
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
        std::env::remove_var("PIRS_TELEGRAM_BOT_TOKEN");
        let err = deliver_outbound(
            &crate::DeliverTarget::Telegram {
                chat_id: "1".into(),
            },
            "hello",
        )
        .await
        .unwrap_err()
        .to_string()
        .to_lowercase();
        assert!(
            err.contains("token") || err.contains("telegram"),
            "expected token error, got {err}"
        );
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
        let main_src = concat!(
            include_str!("../main.rs"),
            include_str!("../bin_helpers/mod.rs"),
            include_str!("../bin_helpers/schedule_fire.rs"),
            include_str!("../bin_helpers/gateway_msg.rs"),
            include_str!("../bin_helpers/chat.rs"),
            include_str!("../bin_helpers/code.rs"),
            include_str!("../bin_helpers/tools.rs"),
            include_str!("../bin_helpers/status.rs"),
        );
        assert!(
            main_src.contains("deliver_outbound(&job.deliver")
                || main_src.contains("deliver_outbound(&j.deliver"),
            "tick/fire must call deliver_outbound with the job deliver target"
        );
        assert!(
            !main_src.contains("if !matches!(j.deliver, DeliverTarget::Cli)"),
            "must not skip Cli deliver after captured subprocess stdout"
        );
        // Outbound Cli arm lives in outbound.rs after the gateway module split.
        let cli_arm = include_str!("outbound.rs");
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
        assert_eq!(
            d.document.as_ref().unwrap().file_name.as_deref(),
            Some("hello.py")
        );
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
        assert_eq!(extract_discord(&v), Some(("99".into(), "hello".into())));
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

    fn webhook_secret_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn webhook_no_secret_allows() {
        let _g = webhook_secret_env_lock();
        std::env::remove_var("PIRS_WEBHOOK_SECRET");
        std::env::remove_var("WEBHOOK_SECRET");
        std::env::remove_var("PIRS_SLACK_WEBHOOK_SECRET");
        assert!(verify_webhook_signature("slack", "POST /\r\n\r\n", "{}").is_ok());
    }

    #[test]
    fn webhook_secret_requires_signature_header() {
        let _g = webhook_secret_env_lock();
        std::env::set_var("PIRS_WEBHOOK_SECRET", "s3cret");
        let err = verify_webhook_signature("slack", "POST /\r\nHost: x\r\n", "{}").unwrap_err();
        assert!(
            err.contains("no X-Pirs") || err.contains("Signature"),
            "{err}"
        );
        std::env::remove_var("PIRS_WEBHOOK_SECRET");
    }

    #[test]
    fn webhook_valid_hmac_passes() {
        let _g = webhook_secret_env_lock();
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

    #[test]
    fn slack_signature_requires_fresh_timestamp() {
        let _g = webhook_secret_env_lock();
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret = b"s3cret";
        let body = r#"{"type":"event_callback"}"#;
        let now = 1_700_000_000i64;
        std::env::set_var("PIRS_WEBHOOK_SECRET", "s3cret");

        // Fresh timestamp + valid v0 HMAC
        let ts = now.to_string();
        let base = format!("v0:{ts}:{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(base.as_bytes());
        let sig = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
        let hdr = format!(
            "POST / HTTP/1.1\r\nX-Slack-Signature: {sig}\r\nX-Slack-Request-Timestamp: {ts}"
        );
        assert!(
            verify_webhook_signature_at("slack", &hdr, body, now).is_ok(),
            "fresh slack sig should pass"
        );

        // Stale timestamp (1 hour old) with matching HMAC for that old ts
        let old = now - 3600;
        let base_old = format!("v0:{old}:{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(base_old.as_bytes());
        let sig_old = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
        let hdr_old =
            format!("POST /\r\nX-Slack-Signature: {sig_old}\r\nX-Slack-Request-Timestamp: {old}");
        let err = verify_webhook_signature_at("slack", &hdr_old, body, now).unwrap_err();
        assert!(err.contains("skew") || err.contains("timestamp"), "{err}");

        // Missing timestamp header
        let hdr_no_ts = format!("POST /\r\nX-Slack-Signature: {sig}");
        let err2 = verify_webhook_signature_at("slack", &hdr_no_ts, body, now).unwrap_err();
        assert!(err2.contains("Timestamp"), "{err2}");
        std::env::remove_var("PIRS_WEBHOOK_SECRET");
    }

    #[test]
    fn parse_content_length_and_header_end() {
        assert_eq!(
            parse_content_length("POST /\r\nContent-Length: 12\r\nHost: x"),
            Some(12)
        );
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nOK";
        assert_eq!(find_header_end(raw), Some(raw.len() - 6)); // before \r\n\r\n... wait
                                                               // "\r\n\r\n" starts at index of double CRLF
        let pos = find_header_end(raw).unwrap();
        assert_eq!(&raw[pos..pos + 4], b"\r\n\r\n");
        assert_eq!(&raw[pos + 4..], b"OK");
    }
}
