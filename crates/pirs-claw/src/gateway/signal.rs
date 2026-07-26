//! signal-cli channel loop.
use std::time::Duration;


use crate::channel::{InboundMessage, CHANNEL_SIGNAL};
use crate::pairing::PairingAllowlist;
use crate::GatewayReply;

use super::allow::require_allowlist;
use super::MessageHandler;


// ─── Signal via signal-cli ──────────────────────────────────────────────────

pub(super) async fn run_signal_cli(
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
