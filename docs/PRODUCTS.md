# pirs products

Two products over one Rust agent core. Everything else is a power tool or pack.

## Portfolio

| Product | Binary | Role | Peers |
|---------|--------|------|--------|
| **Harness** | `pirs` | Multi-model strategies, registry, `--weak`, TUI/RPC/ACP | **pi**, qwen-code core |
| **Agent** | `pirs-claw` | Repo work + chat + schedules + **Telegram-first gateway**; exec local/docker/ssh | Claude Code / Codex CLI; Hermes-class ops (depth over channel count) |

### Power tools (not separate products)

| Tool | Binary | Role |
|------|--------|------|
| Bench | `pirs-bench` | Honest red→green judge |
| Orchestrator | `pirs-orchestrator` | Multi-instance fleet |

## Positioning

**pirs**  
Multi-model agent harness. Plan on a strong model, execute on a cheap one. Strategies, registry, weak-model hardening.  
**Shared core** (also used by claw): agentskills progressive skills + `skill_view`, life tools (`web_fetch`/`web_search`), Soulforge-style `project` toolchain profile (detect + tool + monorepo packages + pre-commit native checks), optional learn/crystallize.

**pirs-claw**  
Always-on personal agent **ops** on that same core: multi-key sessions, schedules, gateway (telegram/…), pairing. Coding/chat use the same tools/skills/learn libraries as `pirs` — claw is not a second stack for those.

Hermes coverage detail: [HERMES-GAPS.md](HERMES-GAPS.md).  
Product roadmap (now / next / later): [ROADMAP.md](ROADMAP.md).

## Mistral Vibe gaps (coding CLI peers)

Closed (achievable without rebuilding Textual):

| Vibe capability | pirs |
|-----------------|------|
| `ask_user_question` | tool `ask_user` (options + labels in tool result) |
| Agent profiles `plan` / `accept-edits` / `auto-approve` | `--agent-profile` / `PIRS_AGENT_PROFILE` enforced at tool gate |
| Session `todo` | tool `todo` (add/update/list, durable under `.pirs/todos.json`) |
| `--worktree` | `--worktree NAME` / `PIRS_WORKTREE` → `.pirs/worktrees/<name>` |

Shipped in the capability upgrade pass (beyond Vibe parity):

| Capability | pirs |
|------------|------|
| Native audit log | `~/.pirs/audit.jsonl`, `audit_tail`, `PIRS_AUDIT` |
| Conversation undo | `/undo`, `session_rewind`, snapshots each user turn |
| LSP diagnostics | `lsp` action=`diagnostics` (+ hover/definition/refs) |
| Blast radius 2-hop | `code_map` action=`blast` |
| PR workflow | tool `pr` (status/diff/create/checks via git+gh) |
| Doctor | `pirs --doctor`, tool `doctor` |
| Research | tool `research` → `.pirs/research/` |
| Fleet | tool `fleet` + `pirs-orchestrator` |
| ACP | image prompts, `fs/read_text_file` / `fs/write_text_file` |
| TUI slash parity | `/model` `/undo` `/doctor` `/audit` `/image` `/profile` `/voice`… |
| Web UI | `pirs --serve` chat SPA |
| Computer use | key + move + click + type + screenshot |

Deferred (non-goals / different product class):

- Full Textual-class chrome (path autocomplete widgets as a product)
- MCP OAuth connector product depth
- Mistral browser sign-in / Mistral-only model lock-in
- New messaging channels
- Email/calendar product (planned later on MCP + life tools)

**Rust vs Rhai:** hard profile denials, tools (`ask_user`, `todo`, `browser_cdp`), and gates stay Rust. Team taste lives in optional packs: `strict-plan.rhai` (extra plan denials), `session-discipline.rhai` (todo/ask_user steering), `browser-cdp-workflow.rhai` (CDP recipes). Packs may only **add** denials, never loosen Rust plan.

## Channel policy

**Focus now:** polish + deep internals on **CLI + Telegram**. No new channel product work.

| Supported | Stubs only (no budget) | Never |
|-----------|------------------------|--------|
| CLI + **Telegram** | discord, slack, whatsapp, signal names | Full OpenClaw 20+ matrix |
| Pairing allowlist (fail closed) | — | Open bots without pairing |
| Coding tools on gateway **opt-in** (`--gateway-code`) | — | Default RCE from chat |
| Telegram single-instance flock | — | Concurrent long-poll on same bot |

```bash
pirs-claw pair add "$CHAT_ID"
pirs-claw serve --channel telegram
```

## What we do not ship

- Desktop knowledge-work suite (Claude Cowork / QoderWork / Codex Work desktop)
- Modal / Daytona / Singularity exec backends
- Full OpenClaw channel zoo beyond the Hermes messaging set
- Honcho dialectic / full skill self-evolution product (partial: skill-crystallizer pack)
- Edge 678KB binary race (nullclaw)
- Production-depth Slack / Discord bots (stubs exist; not the focus)

## Quick commands

```bash
# Harness
pirs --mode tui --strategy plan-exec --model qwen3.5-plus --plan-model deepseek-v4-pro

# Agent — coding
pirs-claw -C ~/repo "fix the failing test"
pirs-claw --exec docker code "run tests in container"
pirs-claw --exec ssh:user@host code "…"

# Agent — chat + memory + skills (agentskills.io progressive)
pirs-claw chat "remind me standup is at 10"
pirs-claw recall "standup"
pirs-claw skills list
pirs-claw skills show my-skill
pirs-claw skills add ./my-skill/
pirs-claw skills install https://example.com/SKILL.md
pirs-claw sessions

# Agent — schedule (durations and/or cron expressions; pause/resume; skill attach)
pirs-claw schedule add --in 1h --every 1d --name pulse --skill my-skill "morning"
pirs-claw schedule add --cron "0 9 * * 1-5" --name standup "weekday brief"
pirs-claw schedule pause pulse
pirs-claw schedule tick --run

# Sessions search (Hermes-class past-chat recall)
pirs-claw sessions search "weather"
pirs-claw status

# Agent — gateway (multi-channel + in-process cron every 60s)
pirs-claw pair add "$CHAT_ID"
pirs-claw serve --channel telegram
pirs-claw serve --channel all
# systemd: scripts/pirs-claw-telegram.service
```

Install: `scripts/install.sh` (binaries: `pirs`, `pirs-claw`, `pirs-orchestrator`).  
Keys: `~/.pirs/secrets.env` + optional `~/.pirs/config.toml` backends (same as `pirs`).
