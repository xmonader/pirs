# Hermes Agent gaps → pirs-claw coverage

What Hermes ships that we map onto pirs / pirs-claw.  
**Explicitly out of scope:** Singularity, Modal, Daytona; Honcho; desktop; OpenClaw 20+ channel zoo.

Legend: **Spine** = production-usable core · **Stub** = thin · **Skip** = intentional · **Moat** = Hermes-only depth we do not chase.

| Hermes capability | Status | How |
|-------------------|--------|-----|
| CLI chat | **Spine** | `pirs-claw chat` + multi-key sessions |
| Full TUI | **Spine** (harness) | `pirs --mode tui` |
| Telegram | **Spine** | long-poll + flock + pairing |
| Discord / Slack | **Stub** | webhook listeners |
| WhatsApp | **Spine** (thin) | Cloud API + hub verify_token |
| Signal | **Stub** | signal-cli if present |
| Multi-channel daemon | **Spine** | `serve --channel all\|a,b` + 60s cron ticker |
| Pairing allowlist | **Spine** | `pair add/list/remove`; fail closed |
| Cross-channel sessions | **Spine** | `sessions/{channel}/{peer}.jsonl` + meta |
| Cron lifecycle | **Spine** | add/list/pause/resume/remove/run/tick; skill attach; in-gateway tick |
| Cron expressions / no-agent scripts / chaining | **Skip** | durations only |
| Local / Docker / SSH exec | **Spine** | `--exec …` |
| Modal / Daytona / Singularity | **Skip** | rejected |
| FTS memory | **Spine** | memory_bridge + recall tool |
| Memory dialectic (Honcho) / SOUL | **Skip** | moat |
| Skills agentskills.io | **Spine (core)** | `pirs-skills` crate — harness + claw; progressive + skill_view |
| Skills Hub / scanners / /learn marketplace | **Skip** | moat |
| Learning loop (crystallize + memory nudge) | **Spine (core)** | `pirs-skills::learn`; claw gateway opt-in; pirs one-shot |
| Life tools (web) | **Spine (core)** | `pirs-tools::web` in default_tools (harness + claw) |
| Browser / vision / TTS suite | **Skip** / partial | no CDP suite |
| Multi-provider models | **Spine** | user registry + secrets.env |
| Subagents | **Spine** | delegate on code path |
| Trajectories / RL export | **Skip** | use `--trace` / pirs-bench |
| Installer | **Spine** | `scripts/install.sh` includes pirs-claw |
| systemd always-on | **Spine** | example unit + cron-in-serve |

## Env (gateway / learn / tools)

| Var | Role |
|-----|------|
| `TELEGRAM_BOT_TOKEN` | Telegram |
| `PIRS_CLAW_ALLOW_ALL` | Dev: skip pairing (warns) |
| `PIRS_CLAW_PUBLIC_BIND` / `PIRS_CLAW_BIND` | Webhook bind |
| `PIRS_CLAW_LEARN` | Enable learn on gateway |
| `PIRS_CLAW_NO_LEARN` | Disable learn on CLI |
| `PIRS_CLAW_SKILL_WRITE` | `0` denies skill_manage (gateway default) |
| `PIRS_CLAW_ALLOW_PRIVATE_URLS` | Allow SSRF-sensitive fetch |
| `PIRS_CLAW_HTTP_JSON` | Enable http_json tool |
| `PIRS_CLAW_SEARCH_URL` | Custom search URL template `{query}` |
| `WHATSAPP_VERIFY_TOKEN` | Meta hub challenge |

## Exec backends

```bash
pirs-claw --exec local|docker|docker:image|docker@ctr|ssh:user@host code "…"
pirs-claw --exec modal …   # error: unsupported
```
