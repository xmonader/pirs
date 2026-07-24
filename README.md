# pirs

**Two products** over one agent core — see [docs/PRODUCTS.md](docs/PRODUCTS.md):

| Product | Binary | Role |
|---------|--------|------|
| **Harness** | **`pirs`** | Multi-model strategies, registry, `--weak`, TUI/RPC/ACP (peers: **pi**) |
| **Agent** | **`pirs-claw`** | Daily agent: **repo work + chat + schedules + gateway** (telegram/discord/slack/whatsapp/signal); exec local/docker/ssh. Hermes gaps covered except Modal/Daytona/Singularity. |

Power tools (not separate products): `pirs-bench`, `pirs-orchestrator`.

Also: [docs/PRODUCTS.md](docs/PRODUCTS.md) · [docs/pirs-claw.md](docs/pirs-claw.md) · [docs/TUI-JOURNEY.md](docs/TUI-JOURNEY.md) · [docs/shrimp-transfer.md](docs/shrimp-transfer.md)

A Rust port of the [pi agent harness](https://github.com/earendil-works/pi): an OpenAI-compatible coding agent with a streaming agent loop, built-in coding tools, [rhai](https://rhai.rs)-script extensibility, a headless RPC mode, and a multi-instance orchestrator.

Status: **alpha**. The core is ported and tested (150+ tests); Google provider and sandboxing are not yet ported.

Providers: **OpenAI-compatible** (`--provider openai`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`) and **Anthropic** (`--provider anthropic`, `ANTHROPIC_API_KEY`) — both with streaming, tool calling, retries, and thinking-block support.

UI: `--mode tui` (ratatui agent console: first-run tour, slash completion, tool groups, mode-colored composer, approvals, steer-by-typing — see [docs/TUI-JOURNEY.md](docs/TUI-JOURNEY.md)) alongside the plain REPL (default), `--mode web` / `--serve` (browser UI on localhost — full tools/packs/MCP), `--mode rpc` (headless JSONL), and `--mode acp` (Agent Client Protocol, for editors that embed agents directly).

Runtime features: auto-compaction with `/compact`, approval modes (`--approval auto|ask|yolo`, `/approval`), background jobs (`bash`/`delegate` with `background: true`, managed via `jobs`/`job_output`/`job_kill`/`job_steer`), goal support (`goal.rhai` pack), multi-model delegation (`delegate` with `model` override), token+cache accounting (`/usage`), flight recorder (`--trace` / `--trace=PATH` → JSONL under `~/.pirs/traces/` with agent events + strategy phase boundaries; same schema as `pirs-bench --trace`).

## Quickstart

```bash
cargo build --release
export OPENAI_API_KEY=...            # or --api-key; OPENAI_BASE_URL for compatible endpoints

./target/release/pirs                          # interactive REPL
./target/release/pirs --mode tui               # polished agent console (recommended)
./target/release/pirs --mode web               # browser UI (full tools/packs; or --serve)
./target/release/pirs "fix the failing test"   # one-shot

# multi-repo work context (one agent, several roots)
./target/release/pirs --cwd ~/code/frontend --also ~/code/backend --mode tui
# paths: //backend/src/…  or relative (tries each root)
# see docs/WORK-CONTEXT.md
```

### First-time TUI (≈ 60s)

Full walkthrough: **[docs/TUI-JOURNEY.md](docs/TUI-JOURNEY.md)**.

```bash
cd your-repo
pirs --mode tui          # first launch shows Getting started
# empty input: press 1 / 2 / 3 for starters, then Enter
# type / then Tab for commands · ? for keys · /tour to re-show the tour
```

REPL / TUI slash: `/tour`, `/model`, `/plan`, `/act`, `/undo`, `/doctor`, `/audit`, `/image <path>`, `/profile`, `/compact`, `/help`, `/quit`; `!cmd` runs a local command and records it in context (`!!cmd` skips recording). Type while the agent is working to steer it. Sessions persist as JSONL under `~/.pirs/sessions/` (`--resume`). Runtime diagnostics: `pirs --doctor`. Action audit: `~/.pirs/audit.jsonl` (disable with `PIRS_AUDIT=0`).

**Strategies (product set):** `monolithic` (one growing loop on `--model`), `plan-exec` (read-only plan → fresh exec), `plan-critic-exec` / alias `plan-exec-critic` (plan → critic → exec). **Strong plan / weak exec:** `--model <cheap> --plan-model <strong> --strategy plan-exec` (or `plan-critic-exec`) — planning (and critique) run on `--plan-model`, the executor keeps `--model`. Measured results and caveats: [docs/hybrid-model-economics.md](docs/hybrid-model-economics.md).

### Multi-backend models (pin + portable)

Built-in backends: `openrouter`, `dashscope`, `deepseek`, `openai`, `anthropic`, `groq`.
Put keys in `~/.pirs/secrets.env` (or the environment) — no TOML required for common use.

| Mode | Example | Meaning |
|------|---------|---------|
| **Pin** | `--model dashscope/qwen3.5-plus` | One subscription + remote id (split on first `/`) |
| **Pin** | `--model openrouter/deepseek/deepseek-v4-flash` | Remote id may contain more `/` |
| **Portable** | `--model qwen-plus` | Failover across backends that list that model (skip missing keys) |

```bash
echo 'OPENROUTER_API_KEY=…' >> ~/.pirs/secrets.env
echo 'DASHSCOPE_API_KEY=…'  >> ~/.pirs/secrets.env

pirs backends                     # keys + catalog status
pirs models                       # portable names
pirs models refresh               # fetch each active backend's /models
pirs models search deepseek       # search caches → pin strings

pirs --model dashscope/qwen3.5-plus "…"
pirs --model qwen-plus --plan-model openrouter/anthropic/claude-sonnet-4 \
  --strategy plan-exec "fix the failing test"
```

Second OpenRouter account (same API, different name + key) in `~/.pirs/config.toml`:

```toml
[[backends]]
name = "openrouter-work"
kind = "openai_compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_WORK_API_KEY"
```

```bash
pirs --model openrouter-work/deepseek/deepseek-v4-flash "…"
```

Optional `[[models]]` overrides/adds **portable** names (ordered `serve` lists).
Project `.pirs/config.toml` backends load only if trusted (`pirs trust`).

Hardening flags: `--tool-diet`, `--sequential`, `--no-compaction` / `--context-window N`, `--max-retries N`. **Profiles** select extension packs: built-in `default` (`packs: "*"`) loads the full catalog; custom profiles live in `.pirs/profiles/<name>.rhai` (see `extensions/README.md`). Opt out with `--no-extensions`. **`--weak`**: tool-diet + sequential + retries≥3 + default strategy `plan-exec` (one-shot) + larger **repo_map** + **auto-verify** when a test ecosystem is detected (packs stay on `default`). Pair with `--plan-model` for multi-model. **`edit_block`** accepts SEARCH/REPLACE. `delegate` supports a per-subagent `model` override. Auto-compaction summarizes old history. `extensions/weak-model.rhai` adds loop/thrash/stop-gate/plan pins; the host restores protected control pins if a context rewrite drops them.

## Extensions (rhai)

Drop `.rhai` files in `.pirs/extensions/` or `~/.pirs/extensions/`:

```rhai
register_tool("word_count", "Count words", #{
    type: "object",
    properties: #{ text: #{ type: "string" } },
    required: ["text"]
});

fn tool_word_count(args) {
    `${args.text.split(" ").len()} words`   // note: backtick ${} interpolation
}

fn on_tool_call(id, name, args) {
    if name == "bash" && args.command.contains("rm -rf") {
        return #{ block: true, reason: "rejected by policy" };
    }
    ()
}
```

Loop hooks: `on_context(messages)`, `on_should_stop(info)`, `on_steering()`, `on_follow_up()`, `on_event(type, data)` (events carry token usage). State per extension via `state_get`/`state_set`; shell out via `exec(cmd, timeout_secs)`; file append/read via `fs_append`/`fs_read`; register slash commands via `register_command(name, desc)` + `fn cmd_<name>(args)` — dispatched by the REPL.

Shipped packs in `extensions/`:

| Pack | Purpose |
|---|---|
| `weak-model.rhai` | loop detector, edit-thrash / no-progress steer, verify-after-edit, stop gate, plan pinning (also loaded by `--weak`) |
| `sandbox.rhai` | OS-level sandbox for `bash` (bubblewrap/Seatbelt, falls back to Docker/Podman): read-only fs outside cwd, no network (or a domain allowlist via `.pirs/sandbox-allowlist.txt`) |
| `guardrails.rhai` | block destructive bash patterns, ask-first policy |
| `path-guard.rhai` | block sensitive bash commands targeting paths outside cwd, plus `find -exec`/`-delete` |
| `audit-log.rhai` | (optional pack) tool calls to JSONL — **native audit is always on** via `~/.pirs/audit.jsonl` unless `PIRS_AUDIT=0` |
| `conductor.rhai` | strong-planner/weak-executor guidance + plan tool |
| `context-janitor.rhai` | shrink stale giant tool outputs in outgoing context |
| `review-gate.rhai` | independent fresh-context sub-agent reviews every diff (request-compliance + adversarial), can refuse completion |
| `instincts.rhai` | learn failure→fix pairs, steer away from repeats |
| `session-handoff.rhai` | next-session brief carried via `.pirs/handoff.md` |
| `shadow-verify.rhai` | re-runs test commands, flags claimed-vs-actual discrepancies |
| `spec-check.rhai` | pins ACCEPTANCE.md, forces item-by-item verification |
| `semantic-bookmarks.rhai` | model-pinned notes at context tail (max 5) |
| `diff-shield.rhai` | merges consecutive same-tool results to save context |
| `chapter-spine.rhai` | weak-model chapter titles keep long sessions coherent |
| `repo-pulse.rhai` | fresh branch/dirty-files pin after every edit |
| `env-doctor.rhai` | blocks calls to missing toolchains with install hints |
| `cost-sentinel.rhai` | token budget: warn at 50%, hard-stop at cap |
| `critic-arena.rhai` | same task on two models, you judge the answers |
| `relay-race.rhai` | draft→critique→finalize pipeline as one tool |
| `hive-note.rhai` | shared blackboard for multi-instance coordination |
| `critic.rhai` | interleaved mid-run diff review via background sub-agent (steers corrections) |
| `blast-radius-judge.rhai` | semantic blast-radius: sub-agent judges risky commands against the environment |
| `skill-crystallizer.rhai` | distills successful runs into reusable SKILL.md files (self-improving) |
| `stash-checkpoint.rhai` | dangling `git stash create` snapshots per turn (never stages), /undo merges via `git stash apply` |
| `swarm.rhai` | work-packet queue over the hive for multi-instance fleets |
| `goal.rhai` | session goals: pinned, compaction-proof, verified, persisted |
| `telemetry.rhai` | metadata-only usage stats (counts, tokens, stop reasons) — never prompt/tool content |

Loop features: `--cascade <draft_model>` drafts each turn on a cheap model and escalates only when the judge rejects it; `spawn_subagent(task, model, tag)` + `inbox()` let scripts run background sub-agents.

Code graph (`--no-graph` to disable): tree-sitter index of the repo (rust/py/ts/go) powering `code_map` (definitions/callers/callees/top/blast — much cheaper than grep+read), `ast_edit` (replace_function_body/rename_symbol/move_function at symbol level), blast-radius notes appended to edit/write results, and a shared (path,mtime) read cache across main and sub-agents. Rollback snapshots are also tagged as git refs (`refs/pirs/turn-N`).

Scripts can also spawn fresh-context sub-agents themselves: `run_subagent(task, model?)`.

Session memory (`~/.pirs`-adjacent `.pirs/memory.db`, always on): every tool result and every message compaction drops out of context is spilled into a SQLite FTS5 store; the `recall` tool searches it by keyword, scoped to the current session, so a session is effectively unbounded even on a small context window. With `--semantic --embed-model <model>` (the same flags that enable `code_search`'s semantic arm — the embedder is shared between the two), `recall` also supports `mode: "semantic"`: embeds the query, cosine-searches every stored vector across *every past session* (not just the current one — recalling something from a previous run is the actual point), and re-ranks the candidate pool with MMR (maximal marginal relevance) so results aren't just near-duplicates of the single best match. `pirs_agent::memory::MemoryStore` also exposes `consolidate` — merges near-identical memories (by cosine similarity) accumulated across sessions, always keeping the more recent of a pair — for projects that run long enough to build up repetitive recurring-error memories.

rhai gotchas (pinned by tests): interpolation only in backtick strings `` `${x}` `` (and backtick strings don't process `\n`/other backslash escapes at all — only real embedded newlines and `${}` interpolation; use a normal `"..."` string for escapes); string methods like `trim()` mutate in place; no `let mut`; arrays have no `join` — use `str_join(arr, sep)` or a loop; array property access clones (write whole entries back); a top-level `const`/`let` is only visible inside the one function the host calls directly (via `call_fn`) — a function called *from* that function (nested closures included) can't see it, so pass it as a parameter or make it a local instead.

## MCP servers

pirs is an MCP client (stdio transport). Declare servers in `.mcp.json` (project) or `~/.pirs/mcp.json`, Claude-Code format:

```json
{
  "mcpServers": {
    "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"] }
  }
}
```

Remote servers work too — streamable HTTP (any `url`) and legacy HTTP+SSE (`url` ending in `/sse` or `"type": "sse"`), with `headers` for auth and `${ENV_VAR}` interpolation in url/headers/args/env:

```json
{
  "mcpServers": {
    "marketplace-srv": {
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer ${MCP_TOKEN}" }
    }
  }
}
```

Server tools appear as `mcp_<server>_<tool>` and are full citizens: schema validation, policy hooks (guardrails apply), usage accounting. `--no-mcp` disables. Prompt caching: `prompt_cache_key` is sent to api.openai.com; the usage line reports cache hit rate.

## Skills & commands (.claude / .agents / .pirs)

Standard conventions are honored at startup, project dir first then `$HOME`:

- **Skills**: `SKILL.md` (with `name`/`description` frontmatter) in `.claude/skills/`, `.agents/skills/`, `.pirs/skills/` — injected as an `<available_skills>` block; the model loads the file via `read` when relevant (progressive disclosure).
- **Commands**: `*.md` in `.claude/commands/`, `.agents/commands/`, `.pirs/commands/` — become `/name` slash commands; `$ARGUMENTS` is substituted with the text after the command.
- **Context**: `AGENTS.md` / `CLAUDE.md` in the project root are appended to the system prompt.

## Config file

`--model`/`--provider`/`--base-url`/`--approval` can be pinned in a TOML file
instead of retyping the flag every run: `.pirs/config.toml` (project, nearest
ancestor of cwd wins) sits above `~/.pirs/config.toml` (user), both below
whatever a CLI flag or env var already set. `--show-config` prints where each
of the four settings actually came from (`cli flag` / `env var` / `project
config` / `user config` / `default`):

```toml
# .pirs/config.toml
model = "gpt-5-mini"
provider = "openai"
approval = "ask"
```

`base_url`/`approval` are security-relevant (redirect API traffic / disable
the approval gate), so they are only ever read from the **user** layer
(`~/.pirs/config.toml`) — a cloned repo's own project-level `.pirs/config.toml`
cannot set them, just by being checked out and run. `model`/`provider` carry
no such risk and stay project-configurable.

Values support the same mini-DSL as MCP server config: `${VAR}` expands from
the environment (`$$` escapes to a literal `$`), and a leading `!` runs the
rest of the string as a shell command, using its trimmed stdout (`!!` escapes
to a literal leading `!`) — e.g. `provider = "!gh auth token"`.

## GitHub Action

`action.yml` at the repo root runs pirs as a one-shot GitHub Action —
downloads the matching release binary for the runner's platform (from this
repo's own tagged releases; see `.github/workflows/release.yml`) and invokes
it non-interactively:

```yaml
- uses: xmonader/pirs@v0.1.0
  with:
    prompt: "fix the failing test in src/foo.rs"
    provider: openai            # or anthropic; base-url below for non-OpenAI endpoints
    model: gpt-5-mini
    api-key: ${{ secrets.OPENAI_API_KEY }}
```

`--approval auto` is always forced (never prompts). `base-url` (for
OpenAI-compatible non-OpenAI endpoints), `max-turns`, and `strategy` are
optional passthroughs.

## ACP (Agent Client Protocol)

`--mode acp` speaks [ACP](https://agentclientprotocol.com) — JSON-RPC 2.0
over newline-delimited JSON on stdio — so editors that embed agents
directly (Zed, and others as the ecosystem grows) can drive pirs instead of
going through a terminal:

```bash
pirs --mode acp
```

Implemented: `initialize`, `session/new`, `session/prompt`, `session/cancel`;
streamed `session/update` notifications (`agent_message_chunk` for assistant
text, `tool_call`/`tool_call_update` for tool execution); every tool call is
gated through the client via `session/request_permission` — there's no
local auto/yolo/ask distinction in this mode. **Not yet implemented**:
`fs/read_text_file`/`fs/write_text_file` (pirs's tools read/write the real
filesystem directly, so an editor's unsaved-buffer content isn't visible to
it), `terminal/*`, `session/load`, `authenticate`, and multiple concurrent
sessions (a second `session/new` replaces the current one). See
`crates/pirs/src/acp_mode.rs` for the full scope notes.

## Orchestrator

Run fleets of headless agents (`pirs --mode rpc`, pi-compatible JSONL RPC):

```bash
pirs-orchestrator serve &
pirs-orchestrator spawn --cwd /repo --label demo
pirs-orchestrator rpc <id> '{"type":"prompt","message":"run the tests"}'
pirs-orchestrator rpc-stream <id>        # raw JSONL bridge
pirs-orchestrator stop <id>
```

## Crates

| Crate | Contents |
|---|---|
| `pirs-ai` | message types, OpenAI-compatible SSE streaming client, tool-call accumulation, retries |
| `pirs-agent` | agent loop, tool execution, hooks, events, steering/follow-up queues |
| `pirs-tools` | `bash`, `read`, `edit`, `write`, `grep`, `find`, `ls` |
| `pirs-rhai` | rhai extension host: script tools, tool policy, loop hooks |
| `pirs` | CLI (`--mode repl\|tui\|web\|rpc\|acp`) |
| `pirs-mcp` | MCP stdio client: JSON-RPC lifecycle, `mcp_*` tool adapter |
| `pirs-orchestrator` | daemon + CLI for spawning/managing headless instances |

## Development

```bash
make build   # cargo build
make test    # cargo test --workspace
make lint    # clippy -D warnings
```

## Notable divergences from pi

- OpenAI-compatible providers only (for now); grep/find are native Rust instead of rg/fd binaries; fuzzy `edit` is line-based, escalating from exact match through quote/dash/trailing-whitespace normalization to full reflow (indentation- and internal-spacing-insensitive) before failing; compaction is trigger-based (no model-aware tokenizer); no radius cloud presence; MIT licensed.
