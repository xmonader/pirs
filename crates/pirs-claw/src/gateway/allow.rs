//! Pairing allowlist gate.
use crate::pairing::PairingAllowlist;


pub(super) fn require_allowlist(allowlist: &PairingAllowlist, channel: &str) -> anyhow::Result<()> {
    if allowlist.is_empty() {
        anyhow::bail!(
            "{channel}: pairing allowlist is empty (fail closed).\n\
             Add peer ids to ~/.pirs/claw/allowlist.txt (one per line), or set \
             PIRS_CLAW_ALLOW_ALL=1 for local dev only."
        );
    }
    Ok(())
}
