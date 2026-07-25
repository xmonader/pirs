//! Pairing allowlist gate.
use std::path::Path;

use crate::pairing::PairingAllowlist;

/// Fail closed unless allowlist has peers, allow-all is on, or pending pairing codes exist.
pub(super) fn require_allowlist(allowlist: &PairingAllowlist, channel: &str) -> anyhow::Result<()> {
    require_allowlist_for_state(allowlist, channel, None)
}

pub(super) fn require_allowlist_for_state(
    allowlist: &PairingAllowlist,
    channel: &str,
    state_dir: Option<&Path>,
) -> anyhow::Result<()> {
    if !allowlist.is_empty() {
        return Ok(());
    }
    // Empty allowlist is OK when owner minted pending codes (self-pair flow).
    if let Some(dir) = state_dir {
        let codes = dir.join("pairing-codes.json");
        if codes.is_file() {
            if let Ok(text) = std::fs::read_to_string(&codes) {
                if text.contains('"') && text.len() > 10 {
                    return Ok(());
                }
            }
        }
    }
    anyhow::bail!(
        "{channel}: pairing allowlist is empty (fail closed).\n\
         Add a peer: `pirs-claw pair add <chat_id>`\n\
         Or mint a code: `pirs-claw pair code` (unpaired peer DMs the code)\n\
         Or set PIRS_CLAW_ALLOW_ALL=1 for local dev only."
    )
}
