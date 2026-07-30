//! Channel pairing / allowlist (Hermes + OpenClaw lesson: never open bots to the world).
//!
//! Supports both raw peer ids and short **pending codes** (`pirs-claw pair code`):
//! an unpaired user DMs the code; the gateway redeems it and adds their chat id.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Env var that disables pairing (dev only).
pub const ALLOW_ALL_ENV: &str = "PIRS_CLAW_ALLOW_ALL";

/// Warning printed when allow-all is active (must stay stable for tests/docs).
pub const ALLOW_ALL_WARNING: &str =
    "WARNING: PIRS_CLAW_ALLOW_ALL is set — pairing disabled; any peer can talk to this gateway (dev only)";

/// Default TTL for pending pairing codes (seconds).
pub const DEFAULT_CODE_TTL_SECS: u64 = 600;

/// True when `PIRS_CLAW_ALLOW_ALL` is 1/true (case-insensitive).
pub fn allow_all_enabled() -> bool {
    std::env::var(ALLOW_ALL_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Emit dangerous-dev warning to stderr if allow-all is on. Returns whether warned.
pub fn warn_if_allow_all() -> bool {
    if allow_all_enabled() {
        eprintln!("[pirs-claw] {ALLOW_ALL_WARNING}");
        true
    } else {
        false
    }
}

/// Normalize peer input: trim, strip common channel prefixes.
pub fn normalize_peer_id(peer: &str) -> String {
    let p = peer.trim();
    for prefix in [
        "telegram:",
        "tg:",
        "discord:",
        "slack:",
        "whatsapp:",
        "signal:",
    ] {
        if let Some(rest) = p
            .strip_prefix(prefix)
            .or_else(|| p.strip_prefix(&prefix.to_ascii_uppercase()))
        {
            return rest.trim().to_string();
        }
    }
    p.to_string()
}

/// Allowlist file: one peer id per line (`chat_id`, Discord user id, …).
/// Lines starting with `#` ignored. Empty file / missing = **deny all** for
/// non-CLI channels (fail closed).
#[derive(Debug, Clone, Default)]
pub struct PairingAllowlist {
    peers: HashSet<String>,
    /// When true, any peer is allowed (dev only). Set via `PIRS_CLAW_ALLOW_ALL=1`.
    allow_all: bool,
}

impl PairingAllowlist {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let allow_all = allow_all_enabled();
        let mut peers = HashSet::new();
        if path.is_file() {
            let text = fs::read_to_string(path)?;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                peers.insert(normalize_peer_id(line));
            }
        }
        Ok(PairingAllowlist { peers, allow_all })
    }

    pub fn default_path(state_dir: &Path) -> PathBuf {
        state_dir.join("allowlist.txt")
    }

    pub fn is_allowed(&self, peer_id: &str) -> bool {
        if self.allow_all {
            return true;
        }
        let p = normalize_peer_id(peer_id);
        self.peers.contains(&p)
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty() && !self.allow_all
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn allow_all(&self) -> bool {
        self.allow_all
    }

    /// Sorted list of paired peer ids (file contents, not allow_all).
    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<_> = self.peers.iter().cloned().collect();
        v.sort();
        v
    }

    /// Add a peer and rewrite the allowlist file.
    pub fn add(&mut self, path: &Path, peer: &str) -> anyhow::Result<bool> {
        let peer = normalize_peer_id(peer);
        if peer.is_empty() {
            anyhow::bail!("peer id must be non-empty");
        }
        let inserted = self.peers.insert(peer);
        self.save(path)?;
        Ok(inserted)
    }

    /// Remove a peer and rewrite the allowlist file. Returns true if it was present.
    pub fn remove(&mut self, path: &Path, peer: &str) -> anyhow::Result<bool> {
        let removed = self.peers.remove(normalize_peer_id(peer).as_str());
        self.save(path)?;
        Ok(removed)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut lines: Vec<_> = self.peers.iter().cloned().collect();
        lines.sort();
        let mut body = String::from(
            "# pirs-claw pairing allowlist — one peer id per line (chat_id / user id)\n\
             # Or: pirs-claw pair code  → user DMs the code to self-pair\n",
        );
        for p in lines {
            body.push_str(&p);
            body.push('\n');
        }
        fs::write(path, body)?;
        Ok(())
    }
}

// ── Pending pairing codes ───────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn codes_path(state_dir: &Path) -> PathBuf {
    state_dir.join("pairing-codes.json")
}

/// Pending code store: code → expiry unix secs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PendingCodesFile {
    /// code (uppercase) → expires_at unix
    codes: HashMap<String, u64>,
}

fn load_codes(path: &Path) -> PendingCodesFile {
    let Ok(text) = fs::read_to_string(path) else {
        return PendingCodesFile::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_codes(path: &Path, file: &PendingCodesFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(file)?)?;
    Ok(())
}

/// Mint a short pairing code the unpaired peer can DM to self-join.
/// Returns the code (always uppercase A–Z0–9, length 6).
pub fn mint_pairing_code(state_dir: &Path, ttl_secs: u64) -> anyhow::Result<String> {
    let path = codes_path(state_dir);
    let mut file = load_codes(&path);
    let now = now_secs();
    file.codes.retain(|_, exp| *exp > now);
    let ttl = ttl_secs.clamp(60, 86_400);
    // 6 chars from base32-ish alphabet without ambiguous 0/O/1/I
    const ALPHA: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut code = String::with_capacity(6);
    for _ in 0..6 {
        let b = getrandom_u32()? % ALPHA.len() as u32;
        code.push(ALPHA[b as usize] as char);
    }
    file.codes.insert(code.clone(), now.saturating_add(ttl));
    save_codes(&path, &file)?;
    Ok(code)
}

/// If `text` is a valid pending code, consume it and add `peer_id` to the allowlist.
/// Returns `Ok(true)` when redeemed, `Ok(false)` when text is not a live code.
pub fn try_redeem_pairing_code(
    state_dir: &Path,
    allowlist_path: &Path,
    text: &str,
    peer_id: &str,
) -> anyhow::Result<bool> {
    let code = text.trim().to_ascii_uppercase();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(false);
    }
    let path = codes_path(state_dir);
    let mut file = load_codes(&path);
    let now = now_secs();
    file.codes.retain(|_, exp| *exp > now);
    let Some(_exp) = file.codes.remove(&code) else {
        save_codes(&path, &file)?; // prune
        return Ok(false);
    };
    save_codes(&path, &file)?;
    let mut al = PairingAllowlist::open(allowlist_path)?;
    let _ = al.add(allowlist_path, peer_id)?;
    Ok(true)
}

/// True if text looks like a pairing code shape (for gateway messaging).
pub fn looks_like_pairing_code(text: &str) -> bool {
    let t = text.trim();
    t.len() == 6 && t.chars().all(|c| c.is_ascii_alphanumeric())
}

fn getrandom_u32() -> anyhow::Result<u32> {
    // Prefer getrandom if available via std; fall back to time+pid mix.
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        if let Ok(mut f) = fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 4];
            if f.read_exact(&mut buf).is_ok() {
                return Ok(u32::from_le_bytes(buf));
            }
        }
    }
    let t = now_secs();
    let p = std::process::id();
    Ok(((t as u32).wrapping_mul(0x9E37_79B9)) ^ p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_by_default() {
        // Ensure allow-all is off for this process for the assertion.
        std::env::remove_var(ALLOW_ALL_ENV);
        let dir = tempfile::tempdir().unwrap();
        let al = PairingAllowlist::open(&dir.path().join("missing.txt")).unwrap();
        assert!(!al.is_allowed("123"));
        assert!(al.is_empty());
        assert!(!al.allow_all());
    }

    #[test]
    fn allowlisted_peer() {
        std::env::remove_var(ALLOW_ALL_ENV);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("allowlist.txt");
        fs::write(&p, "# comment\n42\n99\n").unwrap();
        let al = PairingAllowlist::open(&p).unwrap();
        assert!(al.is_allowed("42"));
        assert!(!al.is_allowed("7"));
        assert_eq!(al.len(), 2);
    }

    #[test]
    fn allow_all_warning_text_is_stable() {
        assert!(ALLOW_ALL_WARNING.contains("PIRS_CLAW_ALLOW_ALL"));
        assert!(ALLOW_ALL_WARNING.contains("pairing disabled"));
    }

    #[test]
    fn add_list_remove_roundtrip() {
        std::env::remove_var(ALLOW_ALL_ENV);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("allowlist.txt");
        let mut al = PairingAllowlist::open(&p).unwrap();
        assert!(al.add(&p, "111").unwrap());
        assert!(!al.add(&p, "111").unwrap()); // already present
        assert!(al.add(&p, "222").unwrap());
        assert_eq!(al.list(), vec!["111".to_string(), "222".to_string()]);
        // Reload from disk
        let al2 = PairingAllowlist::open(&p).unwrap();
        assert!(al2.is_allowed("111"));
        assert!(al2.is_allowed("222"));
        let mut al3 = PairingAllowlist::open(&p).unwrap();
        assert!(al3.remove(&p, "111").unwrap());
        assert!(!al3.is_allowed("111"));
        assert!(al3.is_allowed("222"));
    }

    #[test]
    fn normalize_strips_channel_prefix() {
        assert_eq!(normalize_peer_id("telegram:42"), "42");
        assert_eq!(normalize_peer_id("  99  "), "99");
    }

    #[test]
    fn pairing_code_mint_and_redeem() {
        std::env::remove_var(ALLOW_ALL_ENV);
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path();
        let allow = state.join("allowlist.txt");
        let code = mint_pairing_code(state, 600).unwrap();
        assert_eq!(code.len(), 6);
        assert!(looks_like_pairing_code(&code));
        assert!(!looks_like_pairing_code("hello"));
        // Wrong peer text does not pair
        assert!(!try_redeem_pairing_code(state, &allow, "ZZZZZZ", "peer-1").unwrap());
        // Redeem
        assert!(try_redeem_pairing_code(state, &allow, &code, "peer-1").unwrap());
        let al = PairingAllowlist::open(&allow).unwrap();
        assert!(al.is_allowed("peer-1"));
        // Code is single-use
        assert!(!try_redeem_pairing_code(state, &allow, &code, "peer-2").unwrap());
    }
}
