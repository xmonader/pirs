# pirs products

Two products over one Rust agent core. Everything else is a power tool or pack.

## Portfolio

| Product | Binary | Role | Peers |
|---------|--------|------|--------|
| **Harness** | `pirs` | Multi-model strategies, registry, `--weak`, TUI/RPC/ACP | **pi**, qwen-code core |
| **Agent** | `pirs-claw` | Repo work + chat + schedules + **gateway** (telegram/discord/slack/whatsapp/signal); exec local/docker/ssh | Claude Code / Codex CLI; Hermes / OpenClaw (thinner on learning/desktop) |

### Power tools (not separate products)

| Tool | Binary | Role |
|------|--------|------|
| Bench | `pirs-bench` | Honest red→green judge |
| Orchestrator | `pirs-orchestrator` | Multi-instance fleet |

## Positioning

**pirs**  
Multi-model agent harness. Plan on a strong model, execute on a cheap one. Strategies, registry, weak-model hardening.  
**Shared core** (also used by claw): agentskills progressive skills + `skill_view`, life tools (`web_fetch`/`web_search`), optional learn/crystallize.

**pirs-claw**  
Always-on personal agent **ops** on that same core: multi-key sessions, schedules, gateway (telegram/…), pairing. Coding/chat use the same tools/skills/learn libraries as `pirs` — claw is not a second stack for those.

Hermes coverage detail: [HERMES-GAPS.md](HERMES-GAPS.md).

## Channel policy

| Supported | Never |
|-----------|--------|
| CLI + telegram, discord, slack, whatsapp, signal | Full OpenClaw 20+ matrix |
| Pairing allowlist (fail closed) | Open bots without pairing |
| Coding tools on gateway **opt-in** (`--gateway-code`) | Default RCE from chat |
| Telegram single-instance flock | Concurrent long-poll on same bot |

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

# Agent — schedule (durations; pause/resume; skill attach)
pirs-claw schedule add --in 1h --every 1d --name pulse --skill my-skill "morning"
pirs-claw schedule pause pulse
pirs-claw schedule tick --run

# Agent — gateway (multi-channel + in-process cron every 60s)
pirs-claw pair add "$CHAT_ID"
pirs-claw serve --channel telegram
pirs-claw serve --channel all
# systemd: scripts/pirs-claw-telegram.service
```

Install: `scripts/install.sh` (binaries: `pirs`, `pirs-claw`, `pirs-orchestrator`).  
Keys: `~/.pirs/secrets.env` + optional `~/.pirs/config.toml` backends (same as `pirs`).
