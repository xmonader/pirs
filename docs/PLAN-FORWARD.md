# Plan forward (Phases 0–2)

**As of:** 2026-07-25  
**North star:** coding harness (`pirs`) + Telegram-first personal agent (`pirs-claw`).  
**Not now:** OpenClaw channel zoo, Skills Hub, desktop, Modal/Daytona/Singularity, in-process Gmail/Outlook OAuth.

## Strategy lock

| Do | Do not |
|----|--------|
| Deepen Telegram, schedule, harness, browser/CDP, memory/soul | New messaging channels |
| Honest doctor/status, recoverable state, tests | Checkbox-chase Hermes feature rows |
| MCP for email/calendar | First-party mail/calendar OAuth product |
| Local / docker / ssh exec | Modal / Daytona / Singularity |

## Channel policy

| Channel | Status |
|---------|--------|
| CLI + **Telegram** | **Spine** (production path) |
| Discord / Slack / WhatsApp / Signal | **Stub / thin** — names exist; not production depth |
| 20+ OpenClaw channels | **Never** (by design) |

## Residual gaps (honest)

1. No live first-party Gmail/Outlook OAuth — use **MCP connectors** (`docs/mcp-email-calendar.md`).
2. No Discord/Slack/WhatsApp/Signal production depth — stubs only.
3. No Skills Hub marketplace — local + URL install only.
4. Schedule fires still need an **LLM key** for chat body.
5. Pairing UX is **allowlist ids**, not OpenClaw pairing codes.
6. Secret redaction in audit is pragmatic (key-name denylist), not full DLP.

## Phase map

| Phase | Goal | Status |
|-------|------|--------|
| **0** Stabilize | Docs match code; green gates; push decision logged | This pass |
| **1** Spine depth | Doctor/status honesty; schedule timeout/fail; audit redaction | This pass |
| **2** Daily-driver polish | Smoke script / cold path; status surfaces schedule+telegram+cdp | This pass |
| **3** Optional bets | Deeper Telegram, pairing codes, MCP pack, CUA productization | Deferred |

## Cold-path smoke

```bash
# From repo root (see scripts/viability-smoke.sh)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/home/driver/hero/build/target}
./scripts/viability-smoke.sh
```

## Success metrics

- Doctor/status never prints secret **values** (names only).
- Stub channels labeled in status/docs.
- Schedule fail/timeout path unit-tested.
- Audit redacts obvious secret-shaped tool args.
- Touched-crate `cargo test` green.
