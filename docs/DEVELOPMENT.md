# Development guide

How to build, test, and navigate the pirs workspace. For coding agents, start
with the root **[AGENTS.md](../AGENTS.md)** (also injected into agent system
prompts). For product intent, see [PRODUCTS.md](PRODUCTS.md) and
[ROADMAP.md](ROADMAP.md).

## Prerequisites

- Rust stable (edition 2021 workspace)
- Optional live providers: keys in the environment or `~/.pirs/secrets.env`
- Optional SWE-bench work: Docker + dataset prep (see [SWE-QA.md](SWE-QA.md))

## Clone and build

```bash
git clone https://github.com/xmonader/pirs.git
cd pirs
cargo build --release
```

Binaries land under `target/release/`:

| Binary | Crate | Purpose |
|--------|-------|---------|
| `pirs` | `pirs` | Main harness CLI |
| `pirs-claw` | `pirs-claw` | Personal agent / gateway |
| `pirs-bench` | `pirs-bench-runner` | SWE-style judge + agent driver |
| `pirs-orchestrator` | `pirs-orchestrator` | Multi-instance fleet daemon |

```bash
make build    # cargo build
make test     # cargo test --workspace
make lint     # clippy -D warnings
make fmt      # rustfmt
```

## Workspace layout

```
crates/
  pirs/              CLI, TUI, REPL, RPC, ACP, web, config, strategy turn runner
  pirs-agent/        Agent loop, strategy engine, thrash, hybrid, gates, events
  pirs-ai/           LLM providers (OpenAI-compat, Anthropic), routing, pricing
  pirs-tools/        Built-in tools (bash, edit, read, …)
  pirs-rhai/         Extension host + embedded strategies/profiles
  pirs-graph/        Code graph, search, code_map, embeds
  pirs-lsp/          LSP client + rename_symbol tool surface
  pirs-mcp/          MCP client (stdio / HTTP / SSE)
  pirs-skills/       Skills discovery + progressive load
  pirs-bench/        Red→green harness (judge only)
  pirs-bench-runner/ pirs-bench CLI that drives the real agent
  pirs-orchestrator/ Fleet over RPC children
  pirs-claw/         Gateway, schedules, pairing, speech
  pirs-audio/        Speech / audio helpers
docs/                Human + design docs (this tree)
extensions/          Embedded Rhai packs (source of truth; compiled into binary)
qa/                  Live QA logs, SWE campaign results
```

### Where to put a change

| Change type | Prefer |
|-------------|--------|
| Loop / thrash / hybrid / strategy engine | `pirs-agent` |
| Built-in strategy **content** (phases, prompts) | `crates/pirs-rhai/builtins/*.rhai` |
| Strategy discovery / aliases | `pirs-rhai/src/builtins.rs` |
| CLI flags, modes, session stats | `pirs` |
| New tool | `pirs-tools` (+ wire in CLI tool set) |
| Optional policy / slash / hooks | `extensions/*.rhai` |
| Provider, model catalog, $ rates | `pirs-ai` |
| Bench honesty rules | `pirs-bench` |
| How bench invokes agent | `pirs-bench-runner` |
| Telegram / schedules | `pirs-claw` only |

**Rule:** mechanism in Rust; taste and phase policy in Rhai. Packs may add
denials, never loosen hard Rust gates.

## Running the harness locally

```bash
export DEEPSEEK_API_KEY=…   # or OPENAI_*, ANTHROPIC_*, DASHSCOPE_*, …

./target/release/pirs --mode tui
./target/release/pirs "explain crates/pirs-agent/src/hybrid.rs"

# multi-model strategies
./target/release/pirs \
  --model deepseek-v4-flash \
  --plan-model deepseek-v4-pro \
  --strategy weak-drive \
  "fix the failing unit test"

./target/release/pirs backends
./target/release/pirs models search deepseek
./target/release/pirs --doctor
```

Config layers (highest wins for most keys): CLI → env → project
`.pirs/config.toml` → user `~/.pirs/config.toml`. Security-sensitive
`base_url` / `approval` are **user-layer only**.

Details: root [README.md](../README.md), [MODELS.md](MODELS.md),
[WORK-CONTEXT.md](WORK-CONTEXT.md).

## Testing strategy

| Scope | Command | When |
|-------|---------|------|
| Workspace | `cargo test --workspace` | Before PR / after shared type changes |
| Agent loop / hybrid | `cargo test -p pirs-agent` | Loop, thrash, hybrid, gates |
| Strategies parse | `cargo test -p pirs-rhai` | Builtin or discovery changes |
| CLI / modes | `cargo test -p pirs` | Flags, stats, system prompt |
| Tools | `cargo test -p pirs-tools` | Tool behavior |
| Bench judge | `cargo test -p pirs-bench` | Red→green honesty |
| Bench selftest (no API) | `pirs-bench selftest --count 50` | Pipeline only |
| Bench selftest + agent | needs keys | End-to-end tiny tasks |

Unit tests must not require network. Live provider checks are opt-in.

### Rhai gotchas (tested)

- String interpolation: backtick `` `…${x}…` `` only
- `trim()` mutates in place
- `call_fn` needs statements cleared on stored ASTs (host already does this)

## Strategies and hybrid

See **[STRATEGIES.md](STRATEGIES.md)**. Short version:

- Built-ins embedded from `crates/pirs-rhai/builtins/`
- Override with `.pirs/strategies/<name>.rhai`
- `weak-drive` = strong plan → weak exec → strong review → weak fixup +
  optional mid-loop thrash→advisor (`hybrid: true` + `--plan-model`)
- Implementation: `pirs-agent/src/hybrid.rs`, wired in `pirs/src/turn.rs`

## Extensions and profiles

Catalog and load order: [extensions/README.md](../extensions/README.md).

```bash
# full catalog (default profile packs: "*")
pirs --mode tui

# composed weak runtime flags (not a different pack set)
pirs --weak "…"

# optional smaller role
pirs --profile weak "…"
```

## SWE-bench QA

Campaigns, strict protocol, leaderboard, how to land results:
**[SWE-QA.md](SWE-QA.md)**.

Quick pointers:

- Leaderboard: `qa/bench-swebench-5x5/LEADERBOARD.md`
- Current weak-drive fifty: `qa/bench-swebench-5x5/results_deepseek_v4_flash_strict_weak_drive_fifty/`
- Always write under `qa/bench-swebench-5x5/results_*` (not only `/tmp`)

## Git hygiene

- Default branch: `main`
- Do not commit: `target/`, secrets, `~/.pirs` session dumps, unredacted traces
- Prefer focused commits; keep large result JSON under the qa tree with reports
- Project state notes: root `PROJECT-STATE.md` (historical; prefer `docs/` for living guides)

## Useful entry files

| Topic | File |
|-------|------|
| CLI flags | `crates/pirs/src/cli.rs` |
| Strategy turn + hybrid wire | `crates/pirs/src/turn.rs` |
| Agent builder | `crates/pirs-agent/src/agent.rs` |
| Hybrid thrash→advisor | `crates/pirs-agent/src/hybrid.rs` |
| Strategy types | `crates/pirs-agent/src/strategy.rs` |
| Builtin registry | `crates/pirs-rhai/src/builtins.rs` |
| weak-drive script | `crates/pirs-rhai/builtins/weak-drive.rhai` |
| System prompt + AGENTS.md inject | `crates/pirs/src/system_prompt.rs` |
| Bench CLI | `crates/pirs-bench-runner/src/main.rs` |

## Docs index

See [docs/README.md](README.md).
