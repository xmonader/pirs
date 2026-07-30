# AGENTS.md — working on pirs

This file is loaded into the system prompt when an agent works in this repo
(`AGENTS.md` / `CLAUDE.md`). Read it first. Longer runbooks live under `docs/`.

## What this repo is

**pirs** is a Rust multi-crate workspace: an OpenAI/Anthropic-compatible coding
agent harness + personal agent (`pirs-claw`) + honest SWE bench harness.

| Product | Binary | Role |
|---------|--------|------|
| Harness | `pirs` | Strategies, multi-model, TUI/REPL/RPC/ACP/web |
| Agent | `pirs-claw` | Chat, schedules, Telegram gateway |
| Power tools | `pirs-bench`, `pirs-orchestrator` | Red→green judge; multi-instance fleet |

**North star:** deepen harness + Telegram-first claw. **Not now:** more chat
channels, Skills Hub, Modal/Daytona, OpenClaw channel zoo.

## Map (start here)

| Need | Path |
|------|------|
| Product overview | `README.md`, `docs/PRODUCTS.md` |
| Build / test / crate map | `docs/DEVELOPMENT.md` |
| Strategies (incl. weak-drive) | `docs/STRATEGIES.md` |
| Models / backends | `docs/MODELS.md` |
| Hybrid economics research | `docs/hybrid-model-economics.md` |
| SWE-bench QA campaigns | `docs/SWE-QA.md`, `qa/bench-swebench-5x5/LEADERBOARD.md` |
| Extensions / packs | `extensions/README.md` |
| Roadmap / do-not list | `docs/ROADMAP.md` |
| Built-in strategies (source) | `crates/pirs-rhai/builtins/*.rhai` |
| Hybrid thrash→advisor (Rust) | `crates/pirs-agent/src/hybrid.rs` |
| Strategy phase engine | `crates/pirs-agent/src/strategy.rs`, `crates/pirs/src/turn.rs` |

## Build & verify

```bash
cargo build --release                 # all workspace members
cargo test --workspace                # full suite
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# targeted (faster while iterating)
cargo test -p pirs-agent
cargo test -p pirs-rhai
cargo test -p pirs --lib
cargo build --release -p pirs
cargo build --release -p pirs-bench-runner   # → target/release/pirs-bench
```

`make build` / `make test` / `make lint` wrap the same commands.

Do **not** commit secrets, raw traces under `~/.pirs/traces/`, or huge
untracked result dumps outside `qa/bench-swebench-5x5/results_*`.

## Architecture (mental model)

```
pirs (CLI / TUI / modes)
  └─ pirs-agent   agent loop, strategies, thrash, hybrid, gates
  └─ pirs-ai      providers, routing, pricing, catalog
  └─ pirs-tools   bash/edit/read/… tool implementations
  └─ pirs-rhai    extension host + embedded strategies/profiles
  └─ pirs-graph / pirs-lsp / pirs-mcp / pirs-skills
pirs-claw         gateway + sessions + schedules (uses same core)
pirs-bench        judge (red→green); runner invokes real agent
```

**Strategies are policy, not mechanism.** Built-ins are Rhai maps embedded via
`include_str!` in `crates/pirs-rhai/src/builtins.rs`. Project override:
`.pirs/strategies/<name>.rhai` shadows the same name.

**Multi-model:** `--model` = default / full-scope executor; `--plan-model` =
readonly plan (and hybrid advisor). Role-split usage is printed when they differ.

## Strategies (product set)

| Name | Aliases | Shape |
|------|---------|-------|
| `monolithic` | | One growing loop on `--model` |
| `plan-exec` | | Readonly plan → full exec |
| `plan-critic-exec` | `plan-exec-critic` | Plan → critic → exec |
| `spark-ember` | `dual`, `soul-dual` | Parallel spark explore → ember code |
| **`weak-drive`** | `advisor`, `weak-strong`, `advise-exec` | Strong plan → weak exec → strong review → weak fixup; `hybrid: true` |

### weak-drive (important)

Agentpy hybrid ideas live **as a pirs strategy**, not a separate stack:

1. Strong plan (`--plan-model`, readonly)
2. Weak exec (`--model`, full tools) + mid-loop thrash→advisor when hybrid is on
3. Strong review → `APPROVE` or `REVISE` (plan retained via `{prev_0}`)
4. Weak fixup — **skipped free** if previous text starts with `APPROVE`

```bash
pirs --model deepseek-v4-flash --plan-model deepseek-v4-pro \
  --strategy weak-drive "fix the failing test"
```

**Product vs bench:** mid-loop thrash→advisor + `ask_advisor` is wired on the
**product** `pirs` path (`hybrid: true` + `--plan-model`). Bench campaigns use
the same 4 phases + dual models; treat thrash-escalate as product-path unless
you confirm the runner wires hybrid too. Details: `docs/STRATEGIES.md`.

Strict SWE Lite-50 headline: **34/50** flash+pro weak-drive
(`qa/bench-swebench-5x5/results_deepseek_v4_flash_strict_weak_drive_fifty/`).

## Conventions when editing

### Rust

- Prefer small, tested changes. Put loop/strategy mechanism in `pirs-agent`;
  policy scripts in `pirs-rhai/builtins` or user `.pirs/strategies/`.
- Provider / pricing / model catalog → `pirs-ai`.
- Tools → `pirs-tools` (or extension packs for optional policy).
- Keep thrash, hybrid, and gate behavior honest: no silent success; no fake
  APPROVE paths that skip real review intent.

### Rhai (strategies + extensions)

- Interpolation: **backtick strings** with `${…}` only (not `"…${…}"`).
- Strategy scripts return a map: `name`, optional `hybrid`, `phases: [...]`.
- Phase fields: `scope` (`readonly`/`full`), `system`, `prompt`, optional
  `skip_if_prev_prefix`, model pins via host `pin_plan_model`.
- Templates: `{issue}`, `{targets}`, `{prev}`, `{prev_0}`, `{verdict}`.
- Packs: `extensions/*.rhai`; load order and profiles → `extensions/README.md`.
- **Never** loosen Rust hard denials from packs; packs may only add denials.

### Tests

- Prefer unit tests next to the change; strategy parse tests live in
  `pirs-rhai` (`all_builtins_parse…`, weak-drive shape asserts).
- Live API smoke is optional and needs keys — do not require network in unit tests.
- After strategy shape changes, run `cargo test -p pirs-rhai`.

### Docs

- User-facing behavior change → update `README.md` and/or the matching `docs/*`.
- New strategy or hybrid semantics → `docs/STRATEGIES.md` + this file's table.
- New SWE campaign → land under `qa/bench-swebench-5x5/results_*`, update
  `LEADERBOARD.md`, include models/cost/tokens/wall. See `docs/SWE-QA.md`.

## Secrets & local state

| Item | Where |
|------|--------|
| API keys | env or `~/.pirs/secrets.env` — never commit |
| User config | `~/.pirs/config.toml` |
| Project config | `.pirs/config.toml` (cannot set `base_url` / `approval`) |
| Sessions | `~/.pirs/sessions/` (gitignored) |
| Traces | `~/.pirs/traces/` or `--trace=PATH` — do not commit raw traces with secrets |

## Do / don't

**Do**

- Keep changes focused; match existing style.
- Run targeted tests before claiming done; full workspace when touching shared types.
- Land SWE results in-repo under `qa/bench-swebench-5x5/results_*`.
- Prefer multi-provider / non-Anthropic pins when exploring hybrid economics
  (DeepSeek flash/pro is the measured weak-drive matrix).

**Don't**

- Rewrite agentpy as a parallel product path — hybrid ideas belong in pirs
  strategies (`weak-drive` + `hybrid.rs`).
- Treat fair/rawids SWE scores as comparable to **strict** (strict = agent never
  sees `test_patch` / F2P names).
- Expand messaging channels or add hermes-checkbox features without an explicit ask.
- Commit `target/`, secrets, or agent session dumps.

## Quick diagnosis

| Symptom | Check |
|---------|--------|
| Strategy not found | Name/alias in `canonicalize_name`; file under `.pirs/strategies/` |
| Plan model ignored | Strategy must have readonly phases; confirm `--plan-model` |
| Hybrid silent | Strategy needs `hybrid: true` **and** `--plan-model`; see `turn.rs` logs |
| APPROVE still runs fixup | Phase needs `skip_if_prev_prefix: "APPROVE"`; review must start with `APPROVE` |
| Review lost the plan | Use `{prev_0}` for phase-0 text, not only `{prev}` |
| Bench env failures | Environment setup is fragile — one instance first (`docs/SWE-QA.md`) |

## When stuck

1. `docs/DEVELOPMENT.md` + crate `lib.rs` / module docs nearest the symptom.
2. Existing tests for the same behavior (search `weak-drive`, `hybrid`, `pin_plan`).
3. QA reports under `qa/bench-swebench-5x5/results_*/REPORT*.md` for measured baselines.
