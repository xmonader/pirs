//! Multi-key session model (Hermes gateway/session inspiration).
//!
//! Identity is `(channel, peer)`. History lives at:
//!   `{state_dir}/sessions/{channel}/{peer}.jsonl`
//!
//! CLI uses `cli` / `local`. Legacy single-file `{state_dir}/session.jsonl`
//! is migrated once into `sessions/cli/local.jsonl` so old history is not lost.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::channel::{InboundMessage, CHANNEL_CLI};

/// One durable chat line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionLine {
    pub ts: u64,
    pub role: String,
    pub text: String,
}

/// Sidecar metadata for resume / ops (Hermes session-inspired, thin).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    pub channel: String,
    pub peer: String,
    pub last_active: u64,
    pub message_count: usize,
    pub path: String,
}

/// Session identity: channel + peer within that channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId {
    pub channel: String,
    pub peer: String,
}

impl SessionId {
    pub fn new(channel: impl Into<String>, peer: impl Into<String>) -> Self {
        SessionId {
            channel: channel.into(),
            peer: peer.into(),
        }
    }

    pub fn cli_local() -> Self {
        SessionId::new(CHANNEL_CLI, "local")
    }

    pub fn from_inbound(m: &InboundMessage) -> Self {
        SessionId::new(&m.channel_id, &m.peer_id)
    }

    /// Sanitize peer for filesystem path segments.
    pub fn peer_slug(&self) -> String {
        self.peer
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Relative key `channel/peer` for memory scoping / display.
    pub fn key(&self) -> String {
        format!("{}/{}", self.channel, self.peer_slug())
    }

    /// Absolute path: `{state}/sessions/{channel}/{peer}.jsonl`
    pub fn path(&self, state_dir: &Path) -> PathBuf {
        state_dir
            .join("sessions")
            .join(&self.channel)
            .join(format!("{}.jsonl", self.peer_slug()))
    }
}

/// File-backed session that survives restarts.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    id: SessionId,
}

impl SessionStore {
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::File::create(&path)?;
        }
        Ok(SessionStore {
            path,
            id: SessionId::cli_local(),
        })
    }

    /// Open the store for a session identity under `state_dir`.
    /// For CLI local, migrates legacy `session.jsonl` once if present.
    pub fn open_for(state_dir: &Path, id: SessionId) -> anyhow::Result<Self> {
        if id.channel == CHANNEL_CLI && id.peer == "local" {
            migrate_legacy_cli_session(state_dir)?;
        }
        let path = id.path(state_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::File::create(&path)?;
        }
        Ok(SessionStore { path, id })
    }

    pub fn open_for_inbound(state_dir: &Path, inbound: &InboundMessage) -> anyhow::Result<Self> {
        Self::open_for(state_dir, SessionId::from_inbound(inbound))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn append(&self, role: &str, text: &str) -> anyhow::Result<()> {
        let line = SessionLine {
            ts: now_secs(),
            role: role.into(),
            text: text.into(),
        };
        // Single-write + exclusive flock so concurrent gateway tasks cannot
        // interleave JSON and brick load() forever (review M-30).
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = f.as_raw_fd();
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if rc != 0 {
                anyhow::bail!("session flock failed on {}", self.path.display());
            }
        }
        let mut buf = serde_json::to_vec(&line)?;
        buf.push(b'\n');
        f.write_all(&buf)?;
        f.flush()?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let _ = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        }
        self.touch_meta()?;
        Ok(())
    }

    /// Sidecar metadata next to the jsonl (`*.meta.json`).
    pub fn meta_path(&self) -> PathBuf {
        let mut p = self.path.clone();
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        p.set_file_name(format!("{stem}.meta.json"));
        p
    }

    pub fn touch_meta(&self) -> anyhow::Result<()> {
        let count = self.load().map(|l| l.len()).unwrap_or(0);
        let meta = SessionMeta {
            channel: self.id.channel.clone(),
            peer: self.id.peer.clone(),
            last_active: now_secs(),
            message_count: count,
            path: self.path.display().to_string(),
        };
        if let Some(parent) = self.meta_path().parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(self.meta_path(), serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }

    pub fn read_meta(&self) -> Option<SessionMeta> {
        let text = fs::read_to_string(self.meta_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn load(&self) -> anyhow::Result<Vec<SessionLine>> {
        let f = match fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        let mut skipped = 0u32;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(l) => out.push(l),
                Err(_) => {
                    // Tolerate a corrupt interior line rather than bricking the peer.
                    skipped += 1;
                }
            }
        }
        if skipped > 0 {
            tracing::warn!(
                path = %self.path.display(),
                skipped,
                "session JSONL had corrupt lines; skipped"
            );
        }
        Ok(out)
    }

    pub fn to_agent_messages(&self) -> anyhow::Result<Vec<pirs_ai::Message>> {
        let lines = self.load()?;
        let mut msgs = Vec::new();
        for l in lines {
            match l.role.as_str() {
                "user" => msgs.push(pirs_ai::Message::user(l.text)),
                "assistant" => {
                    msgs.push(pirs_ai::Message::Assistant(pirs_ai::AssistantMessage {
                        content: vec![pirs_ai::ContentBlock::text(l.text)],
                        ..Default::default()
                    }));
                }
                _ => {}
            }
        }
        Ok(msgs)
    }
}

/// One-shot: if legacy `session.jsonl` exists and multi-key CLI path is empty/missing, copy it.
pub fn migrate_legacy_cli_session(state_dir: &Path) -> anyhow::Result<()> {
    let legacy = state_dir.join("session.jsonl");
    let dest = SessionId::cli_local().path(state_dir);
    if !legacy.is_file() {
        return Ok(());
    }
    if dest.is_file() {
        // Already migrated or has new history — leave both.
        let dest_len = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        if dest_len > 0 {
            return Ok(());
        }
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&legacy, &dest)?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_do_not_share_history() {
        let dir = tempfile::tempdir().unwrap();
        let a = SessionStore::open_for(dir.path(), SessionId::new("telegram", "111")).unwrap();
        let b = SessionStore::open_for(dir.path(), SessionId::new("telegram", "222")).unwrap();
        a.append("user", "secret-from-alice").unwrap();
        a.append("assistant", "hi alice").unwrap();
        b.append("user", "secret-from-bob").unwrap();
        let a_lines = a.load().unwrap();
        let b_lines = b.load().unwrap();
        assert_eq!(a_lines.len(), 2);
        assert_eq!(b_lines.len(), 1);
        assert!(a_lines[0].text.contains("alice"));
        assert!(!a_lines.iter().any(|l| l.text.contains("bob")));
        assert!(!b_lines.iter().any(|l| l.text.contains("alice")));
        // Distinct paths
        assert_ne!(a.path(), b.path());
        assert!(a.path().ends_with("sessions/telegram/111.jsonl")
            || a.path().to_string_lossy().contains("telegram"));
    }

    #[test]
    fn cli_local_migrates_legacy_session_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("session.jsonl");
        {
            let s = SessionStore::open(&legacy).unwrap();
            s.append("user", "legacy-line").unwrap();
        }
        let store = SessionStore::open_for(dir.path(), SessionId::cli_local()).unwrap();
        let lines = store.load().unwrap();
        assert!(
            lines.iter().any(|l| l.text.contains("legacy-line")),
            "expected migrated history: {lines:?}"
        );
        assert!(store.path().to_string_lossy().contains("cli"));
    }

    #[test]
    fn session_id_from_inbound_matches_key() {
        let m = InboundMessage {
            channel_id: "telegram".into(),
            peer_id: "42".into(),
            text: "hi".into(),
            ts: 0,
        };
        let id = SessionId::from_inbound(&m);
        assert_eq!(id.key(), "telegram/42");
        assert_eq!(id.path(Path::new("/s")), PathBuf::from("/s/sessions/telegram/42.jsonl"));
    }

    #[test]
    fn append_writes_meta_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::open_for(dir.path(), SessionId::new("cli", "local")).unwrap();
        s.append("user", "hi").unwrap();
        s.append("assistant", "yo").unwrap();
        let meta = s.read_meta().expect("meta");
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.channel, "cli");
        assert!(meta.last_active > 0);
    }
}
