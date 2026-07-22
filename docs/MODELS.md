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

`/model` — show current (pin vs portable)  
`/model dashscope/qwen3.5-plus` — pin  
`/model qwen-plus` — portable  
