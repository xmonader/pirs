# pirs product roadmap

**As of:** local `main` @ `0d5e669` (2026-07-21 session arc)  
**Remote:** **not published** — `main` is **26 commits ahead of `origin/main`** (local only; not a release).

This roadmap is for implementers and product direction after dual-product hardening, Hermes-class spines, core/claw rebalance, and Soulforge-style project tooling.

---

## 1. Portfolio and ownership

### Products

| Product | Binary | Role |
|---------|--------|------|
| **Harness** | `pirs` | Multi-model coding harness: strategies, registry, `--weak`, TUI/RPC/ACP, packs |
| **Agent** | `pirs-claw` | Always-on personal agent **ops**: chat, multi-key sessions, schedule, gateway, pair |
| **Power tools** | `pirs-bench`, `pirs-orchestrator` | Honest red→green eval; multi-instance fleet — not marketed products |

### Ownership rule (source of truth)

```
Shared core (pirs-tools, pirs-skills, pirs-agent, pirs-ai, …)
  → anything that makes coding/chat agents smarter without Telegram

pirs-claw only
  → gateway, pairing, schedule daemon/tick, multi-key messaging sessions, deliver targets
```

**Rule of thumb:** if both binaries need it by default, and wrong behavior is a product bug or security hole → **Rust core**. If it is “how our team likes to work after the tools exist” → **Rhai pack** (harness first; claw only after extension load is wired).

See also: [PRODUCTS.md](PRODUCTS.md), [HERMES-GAPS.md](HERMES-GAPS.md), [pirs-claw.md](pirs-claw.md).

---

## 2. Now — already on local `main` (done)

Do **not** re-implement these; polish or document only.

### Multi-model / weak harness
- Registry backends + aliases, `--plan-model`, failover serve list  
- `--weak` stack, control pins, tool-pair compaction, auto-verify from project test command  
- Session stats, TUI mid-session model/strategy, traces/telemetry  

### Shared core (harness **and** claw)
| Capability | Crate / surface |
|------------|-----------------|
| Progressive agentskills + `skill_list` / `skill_view` / `skill_manage` | `pirs-skills` |
| Learn: memory nudge + skill crystallize | `pirs-skills::learn` |
| Life tools `web_fetch` / `web_search` (+ optional `http_json`) | `pirs-tools::web` in `default_tools` |
| Soulforge-style **project** profile + tool | `pirs-tools::project` |
| Monorepo `project(action: "packages")` | pnpm/npm workspaces, Cargo members, go.work |
| Pre-commit native checks on `git commit` via bash | config tools only; `PIRS_NO_PRECOMMIT=1` to skip |
| Shell hints → prefer `project(action:…)` | bash success path |

### Claw ops spines
- Telegram long-poll + flock + pairing fail-closed  
- Multi-channel `serve --channel all|a,b` + 60s in-process cron tick  
- Schedule lifecycle: add/list/pause/resume/remove/run/tick; skill attach; human durations  
- Multi-key sessions + meta; WhatsApp verify thin; Discord/Slack **stubs**  

### Retired
- `pirs-work` removed; coding defaults live on claw + shared tools  

### Ops reality
| Item | Status |
|------|--------|
| Local main tip | `0d5e669` |
| vs `origin/main` | **ahead 26** — **unpushed** |
| Branch `feat/weak-model-hardening` | Stale pointer; work is on `main` |

---

## 3. Next — near-term themes (ordered)

Prioritize **publish + harden spines**, not new moats.

| Priority | Theme | Concrete outcomes | Surface | Rust vs Rhai |
|----------|--------|-------------------|---------|--------------|
| **P0** | **Publish local main** | `git push` (or PR) so origin matches product; optional delete `feat/weak-model-hardening` | ops | n/a |
| **P0** | **Telegram production checklist live** | One real bot + systemd unit + secrets mode 600; document failure modes | claw | Rust already |
| **P1** | **Registry single source** | Claw stops maintaining a parallel thin parser; user registry helpers shared (extract from harness config path without claw→pirs binary dep) | core | **Rust** |
| **P1** | **Claw loads extensions (optional)** | `~/.pirs/extensions` on claw chat/code so packs apply; gateway stays fail-closed | claw | host wire **Rust**; packs **Rhai** |
| **P1** | **Policy packs aligned with core** | Refresh `skill-crystallizer.rhai` to match `pirs-skills`; `project-discipline.rhai` (prefer project over raw cargo); document `web-tools.rhai` as legacy | rhai | **Rhai** on Rust tools |
| **P2** | **One deep non-Telegram channel** | Only if a real user needs it: pick **one** of Slack or Discord and take to production depth (signatures, threads) — do not deepen both stubs | claw | **Rust** |
| **P2** | **Cron quality without Hermes scope** | Job `last_run` visibility; optional attach model already exists; **no** cron expressions / no-agent scripts unless forced | claw | **Rust** |
| **P2** | **Bench + weak loop** | Keep pirs-bench as truth; optional wire project profile into more auto-verify ecosystems | bench/core | **Rust** |
| **P3** | **Host APIs for thinner packs** | e.g. `project_profile()` map, `crystallize_skill(text)` callable from Rhai | core + rhai | thin **Rust** surface |

### Suggested sequencing (sprints)

1. **Ship** — push main; cut install/docs note that pirs-claw is first-class.  
2. **Ops** — Telegram go-live checklist for one host.  
3. **Core hygiene** — registry share; extension load on claw chat/code.  
4. **Packs** — discipline/crystallize alignment (Rhai).  
5. **Channel depth** — only under demand.

---

## 4. Later — intentional non-goals (do not roadmap as P0)

These are **Hermes / competitor moats or product classes we skip** unless strategy changes:

1. **Skills Hub / marketplace scanners / skills.sh ecosystem**  
2. **Honcho dialectic user modeling / full SOUL.md product**  
3. **Modal / Daytona / Singularity** exec backends  
4. **Full OpenClaw 20+ messaging matrix** (Matrix, Teams, Feishu, SMS, …)  
5. **Desktop Work suite** (Cowork / QoderWork class)  
6. **Trajectory RL / Atropos-style research export** as a product  
7. **Full browser CDP / computer-use suite** (beyond web_fetch/search)  
8. **Cron expressions, no-agent script jobs, context_from chains** (Hermes cron product depth)

Stubs may remain for Discord/Slack/Signal names without production depth.

---

## 5. Rhai vs Rust for next work

| Work | Prefer |
|------|--------|
| Registry unify, project tool, SSRF, pre-commit gate, gateway, schedule, skill_view install/validate | **Rust** |
| “Prefer project over bash”, crystallize after N edits, team extra pre-commit, persona/tone, optional web re-export | **Rhai** |
| Gateway transport, flock, pairing fail-closed | **Rust only** (never pack-dependent security) |

**Do not** reimplement `detect_profile`, gateway, or SSRF primarily in Rhai — packs are opt-in and last-wins.

---

## 6. One-page priority table

| Priority | Outcome | Owner |
|----------|---------|--------|
| P0 | Origin matches local main (push) | ops |
| P0 | One Telegram production host green | claw |
| P1 | Shared registry load for claw | core |
| P1 | Optional extension load on claw chat/code | claw + rhai |
| P1 | Packs aligned with pirs-skills / project | rhai |
| P2 | One messaging channel to production depth (demand-driven) | claw |
| P2 | Cron visibility polish (not Hermes full cron) | claw |
| P3 | Host APIs for thinner packs | core |
| — | Skills Hub, Honcho, Modal/Daytona, 20+ channels, desktop | **non-goal** |

---

## 7. Success metrics (lightweight)

| Signal | Target |
|--------|--------|
| Publish | `origin/main` includes project/skills/claw spines |
| Harness | `pirs` TUI has `project` + `web_fetch` + skill_view without claw |
| Claw | `serve --channel telegram` + pair + schedule tick without fake replies |
| Security | Empty allowlist fails; pre-commit blocks dirty lint; SSRF default on |
| Honesty | Docs mark Discord/Slack stub; HERMES-GAPS spine/stub/skip accurate |

---

## 8. Doc map

| Doc | Use |
|-----|-----|
| [PRODUCTS.md](PRODUCTS.md) | Portfolio positioning |
| [HERMES-GAPS.md](HERMES-GAPS.md) | Capability matrix vs Hermes |
| [pirs-claw.md](pirs-claw.md) | Claw CLI + gateway + project notes |
| [telegram-checklist.md](telegram-checklist.md) | Production Telegram |
| This roadmap | What to build next (and what not to) |

---

*Implementation of Next items is out of scope for this document.*
