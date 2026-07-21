# Hermes Agent gaps → pirs-claw coverage

What Hermes ships that we map onto pirs / pirs-claw.  
**Product focus:** depth + polish on spines we already have (Telegram, harness, schedule, speech, browser/CDP, learn) — **not** more channels.  
**Explicitly out of scope:** Singularity, Modal, Daytona; Honcho SaaS; desktop app; OpenClaw 20+ channel zoo; **Discord/Slack/WhatsApp/Signal production depth** (stubs only, zero budget).

Legend: **Spine** = production-usable path · **Stub** = thin · **Skip** = intentional  
“Spine” ≠ Hermes-depth; roadmap is to **deepen** spines, not multiply surfaces.

| Hermes capability | Status | How |
|-------------------|--------|-----|
| CLI chat | **Spine** | `pirs-claw chat` + multi-key sessions |
| Full TUI | **Spine** (harness) | `pirs --mode tui` |
| Telegram | **Spine** | long-poll + flock + pairing + STT + attachments |
| Discord / Slack | **Stub** | deferred |
| WhatsApp / Signal | **Thin** | present, not productized |
| Multi-channel daemon | **Spine** | `serve` + 60s cron ticker |
| Pairing allowlist | **Spine** | fail closed |
| Sessions + search | **Spine** | JSONL + `sessions search` + `session_search` tool |
| Cron intervals + expressions | **Spine** | `--every` / `--cron` |
| Cron **blueprints** | **Spine** | `schedule blueprint list`, `--blueprint morning-brief` |
| Cron **NL schedule** | **Spine** | `--nl "weekdays at 9:00"` |
| Local / Docker / SSH | **Spine** | `--exec` |
| Modal / Daytona / Singularity | **Skip** | rejected |
| FTS memory + nudge | **Spine** | recall + durable extract |
| **Soul / user profile** | **Spine** | `~/.pirs/soul.md`, `soul show/set/curator`, prompt inject |
| Skill crystallize + improve | **Spine** | learn loop |
| Skills Hub | **Skip** | moat |
| Browser navigate / screenshot | **Spine** | `browser_navigate`, `browser_screenshot` (Chromium/HTTP) |
| Browser **CDP** (Playwright-compatible) | **Spine** | `browser_cdp` via pure-Rust **chromiumoxide** (no Node) |
| Vision | **Spine** | `vision_describe` (OpenAI-compatible VL) |
| Computer use | **Spine** (opt-in) | `PIRS_COMPUTER_USE=1` + scrot/xdotool |
| Multi-provider + multi-backend STT/TTS | **Spine** | registry + `pirs-audio` + Groq |
| Outbound attachments | **Spine** | `attach_file` + Telegram sendDocument |
| Subagents / multi-model coding | **Spine / moat** | strategies, weak, graph, bench |
| Runtime status | **Spine** | `pirs-claw status` |

## CLI

```bash
# Learning / identity
pirs-claw soul show
pirs-claw soul curator
echo "- name: Ada" | pirs-claw soul set

# Cron product
pirs-claw schedule blueprint list
pirs-claw schedule add --blueprint morning-brief --slot time=07:30 --name morning
pirs-claw schedule add --nl "weekdays at 9:00" --name standup "standup"
pirs-claw schedule add --cron "*/15 * * * *" "heartbeat"

# Browser / vision (tools on chat + code)
# computer use: PIRS_COMPUTER_USE=1

# CDP (Playwright / Chrome remote debugging) — pure Rust, no Node
export PIRS_BROWSER_CDP_URL=http://127.0.0.1:9222   # or auto-launch Chromium
# agent tool: browser_cdp action=connect|goto|content|click|type|eval|screenshot|status|close

pirs-claw status
pirs-claw serve --channel telegram
```

### Playwright / Chrome CDP

`browser_cdp` speaks the same **Chrome DevTools Protocol** Playwright uses (`connectOverCDP`).
No Playwright Node runtime is required — we use [chromiumoxide](https://crates.io/crates/chromiumoxide).

```bash
# Option A: attach to existing Chrome/Chromium
chromium --remote-debugging-port=9222 --user-data-dir=/tmp/pirs-cdp &
export PIRS_BROWSER_CDP_URL=http://127.0.0.1:9222

# Option B: attach to Playwright-launched browser
# node -e "require('playwright').chromium.launch({args:['--remote-debugging-port=9222'],headless:false})"
# (browser stays up; pirs connects on 9222)

# Option C: let pirs launch headless Chromium (needs chromium/google-chrome on PATH)
# unset PIRS_BROWSER_CDP_URL

# Disable all browser tools
# PIRS_BROWSER=0
```

Tool actions (JSON args for the agent tool `browser_cdp`):

| action | fields |
|--------|--------|
| `connect` | optional `url` (CDP HTTP endpoint) |
| `goto` | `url` |
| `content` | optional `max_chars` |
| `click` | `selector` |
| `type` | `text`, optional `selector` |
| `eval` | `expression` (JS) |
| `screenshot` | optional `path` (default `.pirs/cdp-shot.png`) |
| `status` / `close` | — |

Feature: `pirs-tools` / `pirs-claw` default **`cdp`**. Build without: `--no-default-features`.

## Env

| Var | Role |
|-----|------|
| `PIRS_CLAW_LEARN` | Learning on gateway |
| `PIRS_SOUL_PATH` | Override soul file |
| `PIRS_BROWSER=0` | Disable browser tools |
| `PIRS_BROWSER_CDP_URL` | CDP endpoint (`BROWSER_CDP_URL` / `CDP_URL` aliases) |
| `PIRS_COMPUTER_USE=1` | Enable desktop screenshot/click/type |
| `PIRS_VISION_MODEL` | VL model id |
| `PIRS_CLAW_TTS_ON_VOICE` | `0` to disable VN TTS default |
| `TELEGRAM_BOT_TOKEN` / speech keys | as before |
