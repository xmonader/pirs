# shrimpswe (pirs port)

Port of `hero_shrimp/support/extbench/shrimpswe`: **our own** agent tasks for
what SWE-bench Lite cannot measure (multi-site completeness, red herrings,
mid-session requirement changes, broken env, needles).

## Contract

1. **Blind prompt** — user complaint only (no file paths in the prompt).
2. Agent works in a **copy** of `fixture/`.
3. **Hidden** acceptance (`checks/<id>.test.ts`) is copied in **after** the run.
4. Fixture’s own tests must stay green (`money` / `csv` / `reports`).
5. **No LLM judge** — deterministic `bun:test` only.

## Tasks

| Task | What it pressures |
|------|-------------------|
| `multi-site` | One root cause in 4 modules — partial fix shows 1/4…4/4 |
| `red-herring` | Dedupe looks correct but mutates caller data two ways |
| `needle` | 14 similar handlers; only one is wrong |
| `broken-env` | Preflight/schema broken before real tests |
| `moving-target` | Prompt 1 design → prompt 2 “OOM, redesign” on same workspace |
| `cross-session` | Turn 2 must recall turn 1 convention (optional; hard) |
| `waiter` | Shrimp ledger internals — **N/A on pirs** (skip) |

## Run

```bash
# needs bun + API keys (e.g. source ~/.pirs/secrets.env)
export CARGO_TARGET_DIR=$HOME/hero/build/target
cargo build -p pirs --release

cd qa/shrimpswe
# default: multi-site red-herring needle broken-env moving-target
# harness uses --cwd + --autonomy full (see run.sh)
./run.sh

# one task
./run.sh multi-site
./run.sh moving-target   # streaming redesign — needs skill streaming-export / strategy lines

# other strategies
STRATEGY=monolithic PLAN_MODEL= MODEL=deepseek-v4-flash ./run.sh multi-site
STRATEGY=plan-exec ./run.sh multi-site red-herring
```

**Recommended product path** (same as root README): `weak-drive` or `plan-exec` +
`--plan-model` + `--autonomy full` for unattended. Working directory: `--cwd` or **`-C`**.

Evidence per task under `runs/pirs-<timestamp>/<task>/`:

- `agent.log` (+ `agent.2.log` for two-turn tasks)
- `model.patch`
- `check.log` / `own-tests.log`
- `workspace/`

## Origin

Upstream README and task design live with hero_shrimp. This tree is the
**fixture + grader + pirs driver** so we can score pirs without the shrimp daemon.
