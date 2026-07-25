# Viability: pirs vs Hermes Agent vs OpenClaw

**As of:** 2026-07-24 · 10-round compare/test/fix pass  
**Product stance:** depth on coding harness + Telegram-first personal agent — **not** channel zoo / Skills Hub / desktop.

## Executive comparison

| Spine | Hermes | OpenClaw | pirs / pirs-claw | Verdict |
|-------|--------|----------|-----------------|---------|
| Coding CLI + tools | Yes | Partial (gateway-first) | **Yes** (`pirs`, strategies, hybrid, graph, bench) | **pirs moat** |
| Multi-model plan/exec | Weak | Weak | **Yes** (`--plan-model`, plan-exec, weak) | **pirs moat** |
| TUI | Yes | No (channels) | **Yes** (`pirs --mode tui`) | Parity |
| Telegram gateway | Yes | Yes | **Yes** (long-poll, flock, pairing fail-closed) | Parity on spine |
| Discord/Slack/WA/Signal | Yes | Yes (many more) | Stub / thin | **Intentional skip** |
| Channel matrix (20+) | Medium | **Product** | No | **OpenClaw wins; we refuse** |
| Cron + NL + blueprints | Yes | Yes (cron) | **Yes** | Parity |
| Schedule fire/fail state | Yes | Yes | **Yes** (`last_status`, `fail_count`, recover_missed) | Parity |
| Skills + learn loop | Yes (+ Hub) | Skills | **Yes** (no Hub) | Parity minus Hub |
| Soul / user model | Yes (+ Honcho) | Memory | **Yes** (`soul.md`) | Parity minus SaaS |
| Browser / CDP | Yes (often Node) | Yes | **Yes** pure-Rust multi-page CDP | Parity+ |
| Computer use | Limited | Nodes | **Yes** opt-in | Parity |
| Office docs | Skills/bash | Skills | **`read` + `office_document` tool** | Parity+ |
| Email/calendar | Channels / MCP | Often channel | **MCP connectors** (documented + mock) | Viable via MCP |
| MCP load + doctor | Yes | Yes | **Yes** (trust gate, doctor lines) | Parity |
| Sandbox / Modal | Modal/Daytona | Docker sandbox | Local/docker/ssh; **no Modal** | Intentional |
| Pairing / DM safety | Yes | Pairing codes | Allowlist fail-closed | Parity (different UX) |

## What “viable alternative” means here

You can run **day-to-day coding + personal Telegram agent + schedules + browser + office + MCP mail/calendar** without Hermes or OpenClaw, with **better multi-model coding economics** than either.

You should **not** expect pirs to replace OpenClaw if your requirement is “20 messaging apps + canvas + mobile nodes.”

## 10-round evidence log

| Round | Focus | Result | Fix |
|-------|--------|--------|-----|
| 0 | Baseline unit suites (tools/mcp/claw/agent) | Green | — |
| 1 | Doctor/status miss `chromium-browser` | Lied “no browser” | Detect `chromium-browser` + snap path |
| 2 | Parallel test pollution (work_context) | Flaky ENOENT on write/read | `clear_work_context` + dead-root fallback |
| 3 | Coding surface gaps vs Hermes | Missing asserts | `office_document` + `browser_cdp` in coding_tools test |
| 4 | Schedule NL/blueprint/pairing CLI | OK | — |
| 5 | Live CDP multi-page + doctor honesty | Live connect ok (0.68s) | Launch flags; status `chrome=` |
| 6 | Schedule tick fire path | Works; Rhai `inbox` warn | Always register `inbox()` without subagent runner |
| 7 | MCP user config load path | 4+ tools from `~/.pirs/mcp.json` | Integration test |
| 8 | Permission: office under workspace-write | Covered | File-mutation classification + test |
| 9 | fail_count recover on success | Hermes-class | `mark_fired` resets fail streak |
| 10 | Full suite re-verify + docs | This file | Commit |

Evidence under `/tmp/pirs-compare-10/`.

## How to run the viability smoke

```bash
export CARGO_TARGET_DIR=/home/driver/hero/build/target
cargo test -p pirs-tools --lib
cargo test -p pirs-mcp
cargo test -p pirs-claw --lib
cargo test -p pirs-claw --test cli_delivery
PIRS_CDP_LIVE=1 cargo test -p pirs-tools --lib live_cdp -- --ignored

# Product paths
pirs --doctor
pirs-claw status
pirs-claw schedule blueprint list
pirs-claw pair add <chat_id>
# Email/calendar mock: see docs/mcp-email-calendar.md
```

## Residual gaps (honest)

1. **No live Gmail/Outlook OAuth product** — MCP only (by design).  
2. **No Discord/Slack/WhatsApp/Signal production depth** — stubs/thin.  
3. **No Skills Hub marketplace.**  
4. **Snap Chromium** needs `--no-sandbox` (we pass it on auto-launch).  
5. **Schedule fires still need an LLM key** for chat body (same class as Hermes).  
6. **Pairing UX** is allowlist ids, not OpenClaw pairing codes.  
7. **Audit secret redaction** is pragmatic key-name denylist, not full DLP.

Plan forward: [PLAN-FORWARD.md](PLAN-FORWARD.md). Smoke: `scripts/viability-smoke.sh`.

## Bottom line

For **Hermes-class personal agent + serious coding harness**, pirs is a **viable alternative** on the spines we claim.  
For **OpenClaw channel matrix**, pirs is **not** a drop-in — and will not become one.
