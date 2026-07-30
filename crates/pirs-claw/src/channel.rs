//! Channel ingress/egress seams (Hermes multi-channel gap).

use std::time::{SystemTime, UNIX_EPOCH};

pub const CHANNEL_CLI: &str = "cli";
pub const CHANNEL_TELEGRAM: &str = "telegram";
pub const CHANNEL_DISCORD: &str = "discord";
pub const CHANNEL_SLACK: &str = "slack";
pub const CHANNEL_WHATSAPP: &str = "whatsapp";
pub const CHANNEL_SIGNAL: &str = "signal";

/// All gateway channel ids (excluding cli).
pub const GATEWAY_CHANNELS: &[&str] = &[
    CHANNEL_TELEGRAM,
    CHANNEL_DISCORD,
    CHANNEL_SLACK,
    CHANNEL_WHATSAPP,
    CHANNEL_SIGNAL,
];

/// Normalized inbound message from any transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub channel_id: String,
    /// Peer identity within the channel (`"local"` for CLI; chat id for Telegram).
    pub peer_id: String,
    pub text: String,
    pub ts: u64,
}

impl InboundMessage {
    pub fn cli(text: impl Into<String>) -> Self {
        InboundMessage {
            channel_id: CHANNEL_CLI.into(),
            peer_id: "local".into(),
            text: text.into(),
            ts: now_secs_pub(),
        }
    }

    /// Session path segment key: `{channel_id}/{peer_id}`.
    pub fn session_key(&self) -> String {
        // Sanitize peer for filesystem (no path separators).
        let peer = self
            .peer_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        format!("{}/{}", self.channel_id, peer)
    }
}

/// Normalized outbound reply to the same channel/peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundReply {
    pub channel_id: String,
    pub peer_id: String,
    pub text: String,
}

impl OutboundReply {
    pub fn to(inbound: &InboundMessage, text: impl Into<String>) -> Self {
        OutboundReply {
            channel_id: inbound.channel_id.clone(),
            peer_id: inbound.peer_id.clone(),
            text: text.into(),
        }
    }
}

/// Minimal channel surface: deliver a reply.
pub trait Channel {
    fn channel_id(&self) -> &str;
    fn deliver(&self, reply: &OutboundReply) -> anyhow::Result<()>;
}

/// Stdout delivery for the CLI transport.
#[derive(Debug, Default, Clone)]
pub struct CliChannel;

impl Channel for CliChannel {
    fn channel_id(&self) -> &str {
        CHANNEL_CLI
    }

    fn deliver(&self, reply: &OutboundReply) -> anyhow::Result<()> {
        println!("{}", reply.text);
        Ok(())
    }
}

pub fn now_secs_pub() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_inbound_session_key() {
        let m = InboundMessage::cli("hello");
        assert_eq!(m.channel_id, CHANNEL_CLI);
        assert_eq!(m.peer_id, "local");
        assert_eq!(m.session_key(), "cli/local");
    }

    #[test]
    fn reply_targets_same_peer() {
        let m = InboundMessage::cli("hi");
        let r = OutboundReply::to(&m, "yo");
        assert_eq!(r.channel_id, CHANNEL_CLI);
        assert_eq!(r.peer_id, "local");
        assert_eq!(r.text, "yo");
    }

    #[test]
    fn session_key_sanitizes_peer() {
        let m = InboundMessage {
            channel_id: "telegram".into(),
            peer_id: "12/34".into(),
            text: "x".into(),
            ts: 0,
        };
        assert_eq!(m.session_key(), "telegram/12_34");
    }
}
