//! Outbound message delivery to all channel backends.
use serde_json::json;

use super::telegram::TelegramBot;

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

pub(super) async fn send_discord(peer: &str, text: &str) -> anyhow::Result<()> {
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

pub(super) async fn send_slack(peer: &str, text: &str) -> anyhow::Result<()> {
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

pub(super) async fn send_whatsapp(peer: &str, text: &str) -> anyhow::Result<()> {
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
