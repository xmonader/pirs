# pirs product roadmap

**As of:** 2026-07-21 (capability upgrade pass)  
**Remote:** local `main` **ahead of `origin/main`** (unpushed) — publish is ops.

**North star (current):** coding harness + personal agent **Yes** on A1–A8 / B1–B7 spines.  
**Not now:** more messaging channels, Skills Hub, desktop, Modal/Daytona, OpenClaw zoo.  
**Later integrations:** email, calendar (connectors on top of MCP + life tools).

### Capability matrix (harness + shared core)

| ID | Capability | Status | How |
|----|------------|--------|-----|
| **A1** | Show diffs / interrupt | **Yes** | `edit`/`write` unified diffs in UI+audit; Ctrl-C cancel; TUI Esc |
| **A2** | Audit log + undo/rewind | **Yes** | native `~/.pirs/audit.jsonl` (`PIRS_AUDIT`); `audit_tail`; `/undo` + `session_rewind` |
| **A3** | LSP types / diagnostics / callers | **Yes** | `lsp` action=`diagnostics`/`hover`/…; `code_map` blast second-hop |
| **A5** | TUI / ACP / slash / images | **Yes** | expanded slash (`/undo` `/doctor` `/audit` `/image`…); ACP image+fs; `/voice` foothold |
| **A6** | PR + multi-agent fleets | **Yes** | `pr` tool (gh); `fleet` + `pirs-orchestrator`; `delegate` subagents |
| **A8** | Safety / errors / MCP / doctor | **Yes** | profiles+ask+todo+worktree; MCP load; `pirs --doctor` + tool `doctor` |
| **B1** | Web app | **Yes** | `pirs --serve` polished SPA (SSE chat, auth, drag-drop note) |
| **B2** | Long-term memory of user | **Strong** | `soul.md` inject + learn durable extract + FTS/semantic recall |
| **B3** | Background research | **Yes** | `research` multi-page digest → `.pirs/research/` |
| **B4** | Browse/click + computer use | **Yes** | `browser_cdp` click/type; CUA screenshot/click/type/key/move (`PIRS_COMPUTER_USE=1`) |
| **B5** | Improves over time | **Yes** | learn crystallize + improve skill + soul merge |
| **B6** | Self-write skills from experience | **Yes** | `maybe_crystallize_skill` / skill-crystallizer pack |
| **B7** | Status / doctor | **Yes** | `pirs --doctor`, tool `doctor`, `pirs-claw status` |

### Evaluation snapshot (2026-07-21)

| Signal | Result |
|--------|--------|
| `cargo check -p pirs` (+ agent/tools/graph/lsp) | green |
| unit libs tools/agent | **109** ok |
| Capability WIP | audit, diffs, LSP diagnostics, blast 2-hop, doctor, PR, research, fleet, ACP fs/image, TUI slash |

**Still later:** live TUI mic (voice foothold documents claw STT path); email/calendar connectors; CDP multi-tab product polish; publish origin.

---

## 0. Strategy lock

| Do | Do not |
|----|--------|
| Deepen **internals** of spines we already ship | Add Discord/Slack/WhatsApp product depth |
| **Polish** UX, reliability, diagnostics, tests, docs | Checkbox-chase Hermes feature rows |
| Make Telegram + harness + schedule + speech + browser **proud** | Expand channel matrix |
| Fix seams between crates (agent loop, tools, gateway, registry) | New product surfaces |

**Depth means:** correct under load, honest errors, recoverable state, tested edges, docs that match code.  
**Polish means:** status/doctor, consistent CLI, fewer footguns, predictable tool results, no “spine that lies.”

Channel stubs may remain named; **zero engineering budget** on them until strategy changes.

See also: [PRODUCTS.md](PRODUCTS.md), [HERMES-GAPS.md](HERMES-GAPS.md), [pirs-claw.md](pirs-claw.md).

---

## 1. Portfolio and ownership

### Products

| Product | Binary | Role |
|---------|--------|------|
| **Harness** | `pirs` | Multi-model coding harness: strategies, registry, `--weak`, TUI/RPC/ACP, packs |
| **Agent** | `pirs-claw` | Always-on ops: chat, sessions, schedule, **Telegram** gateway, pair, speech |
| **Power tools** | `pirs-bench`, `pirs-orchestrator` | Honest red→green; multi-instance fleet |

### Ownership rule

```
Shared core (pirs-tools, pirs-skills, pirs-agent, pirs-ai, …)
  → anything that makes coding/chat agents smarter without Telegram

pirs-claw only
  → gateway, pairing, schedule daemon/tick, multi-key messaging sessions, deliver targets
```

**Rule of thumb:** security / wrong-by-default → **Rust core**. Team taste after tools exist → **Rhai pack**.

---

## 2. Already shipped (do not re-build — deepen)

### Harness
- Registry backends + aliases, `--plan-model`, failover  
- `--weak`, control pins, tool-pair compaction, verify-retry  
- Strategies (plan-exec family), TUI / RPC / ACP, packs, MCP, graph  

### Shared core
- agentskills progressive tools; learn/crystallize; soul  
- life tools; `project` + monorepo packages + pre-commit gates  
- browser navigate/screenshot + **CDP** (chromiumoxide); vision; opt-in computer-use  
- sandbox local / docker / ssh  

### Claw ops
- Telegram long-poll + flock + pairing fail-closed  
- STT/TTS multi-backend + attachments  
- schedule: every / cron / NL / blueprints / tick under serve  
- sessions search; `status`; speech setup  

### Explicit stubs / skips
- Discord / Slack / WhatsApp / Signal: **names only**  
- Modal / Daytona / Singularity: **no**  
- Skills Hub / Honcho SaaS / desktop: **no**  

---

## 3. Next — depth & polish (ordered)

No new channels. Work is **internals first**, then UX polish on the same surfaces.

### P0 — Foundation (ops + truth)

| Theme | Concrete outcomes |
|-------|-------------------|
| **Land the tree** | Commit Hermes-gap/CDP/speech arc; decide push vs keep local |
| **Docs match code** | PRODUCTS / HERMES-GAPS / claw docs: Telegram-first; stubs labeled; CDP/speech accurate |
| **Telegram checklist** | One host green end-to-end (pair, chat, VN, attach, schedule fire, flock) |

### P1 — Internals depth (where “as deep as Hermes” is earned)

| Theme | Why | Concrete depth |
|-------|-----|----------------|
| **Agent loop** | Shared brain for `pirs` + claw | ✅ Loop-level tool-result cap + `errorKind` + cancel incomplete; more: compaction edges, retry policy |
| **Gateway Telegram** | Only channel we care about | ✅ Backoff, HTTP timeouts, offset-after-handle, no silent empty, send retry, lock status, **respawn loop**; more: media edges |
| **Schedule engine** | Cron is a product we already claim | ✅ Atomic store, fail-closed corrupt, `last_error`/`last_status`/`fail_count`, mark_failed, cron lock, status next_due, **miss recovery** |
| **Speech path** | Live but sharp edges | ✅ Mock never blocks; short failover timeout; mock TTS skip; **live health probe** in status |
| **Browser CDP session** | Pure-Rust spine | ✅ Alive reconnect, Drop kill, profile cleanup, goto wait, status alive; more: multi-page if needed |
| **Memory / learn / soul** | Skeleton → organism (local) | ✅ Soul inject in harness; single extract→memory+soul; merge dedupe; gateway crystallize; durable heuristic polish |
| **Harness weak/verify** | Our moat | ✅ Weak compose floor + auto-verify notes/tests; project-profile verify path covered |
| **Compaction / tool retry** | Shared loop | ✅ Estimate-token trigger; shrink oversized tool results pre-compact; one transient tool retry |

### P2 — Polish (same features, better product)

| Theme | Concrete outcomes |
|-------|-------------------|
| **`pirs-claw status` / doctor** | One command: keys present (not printed), pairing, flock, schedule next fire, speech backends, CDP env, model registry |
| **CLI consistency** | Flags, exit codes, help text, JSON vs human where useful |
| **Tool output UX** | Truncation, paths, actionable errors (“set PIRS_BROWSER_CDP_URL…”) |
| **Tests on spines** | Gateway unit/integration; schedule next_fire; CDP unit (no Chrome); speech failover; path/SSRF |
| **TUI / REPL feel** | Mid-session model/strategy already exists — reduce friction, clear errors |

### P3 — Thin enablers (only if they serve depth)

| Theme | Notes |
|-------|-------|
| Rhai host APIs for packs | Only to avoid reimplementing core in packs |
| Observability | Traces/session stats already started — make them default-useful |
| Security review pass | Pairing, flock, SSRF, path containment, gateway-code opt-in |

### Explicitly deferred (do not start)

- Discord / Slack / any second channel productization  
- Skills Hub / marketplace  
- Modal / Daytona / Singularity  
- Desktop app  
- Hermes CUA permission product / multi-tab browser product  
- Full Honcho dialectic  

---

## 4. Suggested sequencing

```
1. Land tree + docs truth
2. Telegram reliability internals (gateway)
3. Agent-loop + tool-result robustness (shared)
4. Schedule durability + visibility
5. Speech failover polish
6. CDP session reliability
7. status/doctor + tests + CLI polish
8. Learn/soul/session-search quality
```

Harness weak/verify polish can interleave with 3–7 — it is core moat, not a channel.

---

## 5. Rhai vs Rust (unchanged)

| Work | Prefer |
|------|--------|
| Gateway, schedule engine, agent loop, SSRF, pairing, CDP session | **Rust** |
| Persona, crystallize *policy*, prefer-project hints | **Rhai** |
| Security-sensitive path | **Rust only** |

---

## 6. One-page priority table

| Priority | Outcome | Owner |
|----------|---------|--------|
| P0 | Tree landed; docs honest; Telegram host green | ops + claw |
| P1 | Gateway + agent loop + schedule + speech + CDP **internals** deep | core + claw |
| P1 | Weak/verify/bench regression quality | harness |
| P2 | status/doctor, CLI, tool errors, spine tests | both |
| P2 | Local learn/soul/session-search quality | core + claw |
| — | More channels, hub, desktop, cloud sandboxes | **non-goal** |

---

## 7. Success metrics

| Signal | Target |
|--------|--------|
| Depth | Telegram path survives disconnect, empty model, bad audio, concurrent flock without lying |
| Honesty | `status` and docs never claim Discord/Slack production |
| Harness | Weak + verify + plan-model loop is boringly reliable |
| Internals | Schedule last_run/error visible; CDP connect/close clean; speech failover order tested |
| Polish | New user: pair → chat → VN → schedule in one sitting without folklore |
| Non-goal | Zero new channel code merged |

---

## 8. Doc map

| Doc | Use |
|-----|-----|
| [PRODUCTS.md](PRODUCTS.md) | Portfolio + channel policy |
| [HERMES-GAPS.md](HERMES-GAPS.md) | Capability matrix (coverage, not parity claims) |
| [pirs-claw.md](pirs-claw.md) | Claw CLI + gateway |
| [telegram-checklist.md](telegram-checklist.md) | Production Telegram |
| This roadmap | **What to deepen next** (and what not to touch) |

---

*Channel expansion is out of scope until this depth agenda is boring.*
