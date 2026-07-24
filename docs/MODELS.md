# Models: pin, portable, catalogs

## Two ways to name a model

| Mode | Syntax | Behavior |
|------|--------|----------|
| **Pin** | `backend/remote-id` | One subscription. Split on the **first** `/` only. |
| **Portable** | bare name (`qwen-plus`) | Ordered failover across backends that list it; skip backends without keys. |

```bash
# pin
pirs --model dashscope/qwen3.5-plus "…"
pirs --model openrouter/deepseek/deepseek-v4-flash "…"

# portable (builtin index + your [[models]])
pirs --model qwen-plus "…"
pirs --model qwen-plus --plan-model openrouter/anthropic/claude-sonnet-4 --strategy plan-exec "…"
```

Research note on strong-plan / weak-exec cost and quality (measured matrices, tool ablations): [hybrid-model-economics.md](./hybrid-model-economics.md).

## Built-in backends

`openrouter`, `dashscope`, `deepseek`, `openai`, `anthropic`, `groq`.

Keys (env or `~/.pirs/secrets.env`):

- `OPENROUTER_API_KEY`
- `DASHSCOPE_API_KEY`
- `DEEPSEEK_API_KEY`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `GROQ_API_KEY`

## Multiple accounts of the same provider

Same kind, **different name + key env**:

```toml
# ~/.pirs/config.toml
[[backends]]
name = "openrouter-work"
kind = "openai_compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_WORK_API_KEY"
```

```bash
pirs --model openrouter-work/deepseek/deepseek-v4-flash "…"
```

## Catalogs (list / refresh / search)

```bash
pirs backends                 # keys + catalog age
pirs models                   # portable index + catalog status
pirs models refresh           # all backends with keys
pirs models refresh openrouter
pirs models search claude     # search caches → pin strings
pirs models list openrouter deepseek/
```

Caches: `~/.pirs/cache/catalogs/<backend>.json` (TTL 24h, override with `PIRS_CATALOG_TTL`).

## Optional portable index override

```toml
[[models]]
alias = "my-flash"
serve = [
  { backend = "openrouter-work", model = "deepseek/deepseek-v4-flash" },
  { backend = "openrouter", model = "deepseek/deepseek-v4-flash" },
  { backend = "dashscope", model = "deepseek-v4-flash" },
]
```

## TUI

| Command | Action |
|---------|--------|
| `/model` | Open **fuzzy model picker** (type to filter, ↑↓, Enter) |
| `/models deepseek` | Picker pre-filtered with query |
| `/models plan` | Fuzzy picker for **plan-model** |
| `/models refresh` | Refresh catalogs for backends with keys |
| `/models refresh openrouter` | One backend |
| `/model dashscope/qwen3.5-plus` | Set pin directly (no picker) |
| `/model qwen-plus` | Set portable directly |
| `/plan-model` | Fuzzy picker for planner |
| `/backends` | List backends + key yes/no |
| `/key NAME=value` | Write `~/.pirs/secrets.env` (mode 600) + set env |
| `/backend add name url ENV` | Append `[[backends]]` to user config |
| `/setup` | Key status + how to configure |

Picker sources: portable index + cached catalogs (`~/.pirs/cache/catalogs/`).  
If the list is thin, run `/models refresh` (or `pirs models refresh`) once.

## CLI setup helpers

```bash
pirs setup
pirs key OPENROUTER_API_KEY=sk-…
pirs key DASHSCOPE_API_KEY sk-…
pirs backend add openrouter-work https://openrouter.ai/api/v1 OPENROUTER_WORK_API_KEY
pirs models refresh
```

## DashScope Coding Plan (User-Agent)

Coding Plan endpoints (`coding-intl.dashscope…`) reject non-agent clients with
`405 … only available for Coding Agents`. pirs sets a coding-agent **User-Agent**
on those backends (catalog + chat).

- Default: `claude-cli/2.0.0` (widely accepted until pirs is allowlisted)
- Override: `PIRS_DASHSCOPE_UA=…` or `PIRS_USER_AGENT=…`
- If `/models` still 405s, pirs falls back to the **static Coding Plan allowlist**
