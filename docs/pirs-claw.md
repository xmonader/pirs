# pirs-claw

**Daily agent** over the pirs core: repo work, chat, schedules, gateway, skills, and life tools.  
Equal peer product to the `pirs` harness (see [PRODUCTS.md](PRODUCTS.md)).  
Hermes gap map: [HERMES-GAPS.md](HERMES-GAPS.md).

## Modes

| Mode | Command | Notes |
|------|---------|--------|
| **Code** | `pirs-claw -C repo "…"` / `code` | plan-exec + progressive skills + life tools |
| **Chat** | `pirs-claw chat "…"` | multi-key session + FTS memory + learn loop |
| **Schedule** | `schedule add/list/pause/resume/remove/run/tick` | durations; skill attach; gateway auto-ticks |
| **Gateway** | `serve --channel telegram\|all\|a,b` | multi-channel + 60s cron ticker |
| **Sessions** | `sessions` | `(channel, peer)` + meta |
| **Skills** | `skills list\|show\|add\|install\|validate\|remove\|usage` | [agentskills.io](https://agentskills.io) |
| **Pair** | `pair list\|add\|remove` | gateway allowlist |
| **Voice** | `transcribe <file>` | external whisper / custom cmd |

## Skills (agentskills.io) — shared core

Implemented in **`pirs-skills`** (same library as the `pirs` harness). Claw only adds CLI management commands.

Layout:

```text
~/.pirs/skills/<name>/SKILL.md
~/.pirs/skills/<name>/references/   # optional
~/.pirs/skills/<name>/scripts/      # optional
```

Frontmatter: required `name` + `description` (agentskills rules); optional `license`, `compatibility`, `metadata`, `allowed-tools`.

**Progressive disclosure:** the system prompt only gets name + description. Full body via:

- agent tools `skill_list` / `skill_view` (and `skill_manage` on CLI)
- `pirs-claw skills show NAME`

```bash
pirs-claw skills add ./my-skill/          # directory with SKILL.md
pirs-claw skills install https://…/SKILL.md
pirs-claw skills validate my-skill
pirs-claw skills remove my-skill
```

## Learning loop

After chat/code turns (default on for CLI; off on gateway unless `PIRS_CLAW_LEARN=1`):

1. **Memory nudge** — if the user message looks durable, extract ≤3 facts into FTS memory  
2. **Skill crystallize** — after substantial transcripts, write a new `~/.pirs/skills/<name>/SKILL.md`

```bash
pirs-claw --no-learn chat "…"     # disable for one run
export PIRS_CLAW_NO_LEARN=1       # disable globally
export PIRS_CLAW_LEARN=1          # enable on gateway
export PIRS_CLAW_SKILL_WRITE=0    # deny skill_manage writes (gateway default)
```

Harness users can still load `extensions/skill-crystallizer.rhai` on **pirs**.

## Life tools — shared core

Implemented in **`pirs-tools::web`** and included in `default_tools` for both products:

Also update extensions README note for web-tools.rhai briefly.

| Tool | Role |
|------|------|
| `web_fetch` | GET public URL → text (HTML stripped, truncated) |
| `web_search` | DuckDuckGo lite (or `PIRS_CLAW_SEARCH_URL` with `{query}`) |
| `http_json` | Opt-in via `PIRS_CLAW_HTTP_JSON=1` |

SSRF: localhost / private IPs blocked unless `PIRS_CLAW_ALLOW_PRIVATE_URLS=1`.

Gateway default tools: **recall + skill_list/view + web_fetch/search** (no bash/write).  
`--gateway-code` adds coding tools.

## Model registry

User `~/.pirs/config.toml` only (same shape as harness). Keys from secrets.env.

## Project toolchain (Soulforge-style)

Shared via `pirs-tools::project` (harness + claw). Marker-file detection yields
`test` / `lint` / `typecheck` / `build` / `format` / `run` commands; injected
into the system prompt and available as the `project` tool:

```text
project(action: "list")
project(action: "test")
project(action: "lint")
project(action: "typecheck", cwd: "packages/api")
```

Ecosystems include Bun/Deno/npm-pnpm-yarn, Cargo, Go, Python (uv/poetry/pip),
.NET, PHP, Ruby, Gradle/Maven, CMake/Make, Zig. Prefer `project` over inventing
shell. Weak auto-verify uses `profile.test` when present.

```text
project(action: "packages")   # monorepo: pnpm/npm workspaces, Cargo members, go.work
```

**Pre-commit:** `bash` running `git commit` first runs native lint+typecheck
(config tools only — not arbitrary package.json scripts). Fail blocks the commit.
Skip with `PIRS_NO_PRECOMMIT=1`.

**Shell hints:** successful `cargo clippy` / `npm test` / etc. append a nudge to
use `project(action: …)` next time.

## Exec backends

```bash
pirs-claw --exec local|docker|docker:image|docker@ctr|ssh:user@host code "…"
```

**Not supported:** Modal, Daytona, Singularity.

## Sessions

```text
~/.pirs/claw/sessions/{channel}/{peer}.jsonl
~/.pirs/claw/sessions/{channel}/{peer}.meta.json
```

## Schedule

```bash
pirs-claw schedule add --in 30s --every 1h --name pulse --skill my-skill "…"
pirs-claw schedule pause pulse
pirs-claw schedule resume pulse
pirs-claw schedule remove pulse
pirs-claw schedule run pulse
pirs-claw schedule tick --run
```

`serve` runs an in-process cron ticker every 60s (flock `locks/cron.lock`).

## Gateway

```bash
pirs-claw pair add YOUR_CHAT_ID
export TELEGRAM_BOT_TOKEN=…
pirs-claw serve --channel telegram
pirs-claw serve --channel all              # every channel with credentials
pirs-claw serve --channel telegram,whatsapp
pirs-claw serve --channel telegram --gateway-code
```

systemd: [../scripts/pirs-claw-telegram.service](../scripts/pirs-claw-telegram.service).  
Checklist: [telegram-checklist.md](telegram-checklist.md).

Webhooks bind **127.0.0.1** by default (`PIRS_CLAW_PUBLIC_BIND=1` / `PIRS_CLAW_BIND=0.0.0.0` to open).

## Intentionally not

| Skip | Why |
|------|-----|
| Modal / Daytona / Singularity | Explicit exclusion |
| Full Hermes Skills Hub / scanners | Local + URL install only |
| Honcho dialectic / SOUL.md product | Out of scope |
| 20+ messaging platforms | Hermes set only |
| Full browser CDP suite | web_fetch/search first |

Keys: `~/.pirs/secrets.env`.
