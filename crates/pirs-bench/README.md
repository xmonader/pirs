# pirs-bench

A trustworthy verification harness for running a coding agent against
SWE-bench-style tasks — and a CLI (`pirs-bench`) that drives the real pirs agent
under it.

The design splits cleanly in two:

- **The agent executes.** It localizes (code graph), edits real files, and
  self-corrects by running tests. This is where "solve the task" happens.
- **The harness judges.** It decides success over a real red→green test flip, so
  the agent's own "I'm done" is only advisory. This crate is the judge.

## Why a separate judge

An agent that grades its own work will drift toward declaring victory. The
harness makes success *unfakeable*:

- **Reproduce before fix** — the target test must actually fail at the base
  commit, or there is nothing to fix.
- **Differential verification** — success means the target flipped red→green
  *and* nothing that was green regressed. Pre-existing failures are ignored, so
  a repo that isn't green at checkout still works.
- **Flaky guard** — an accepted flip is re-confirmed on a second run.
- **0 tests collected = failure** — an empty test run never reads as a pass.
- **Test-file protection** — edits to the test files are reverted before
  verification, so a fix cannot pass by weakening the test.
- **Typed attribution** — every non-solve is bucketed (runner-undetected,
  env-setup, repro-failed, fix-no-flip, regressed, flaky, …) so a batch shows
  exactly *where* it loses.

## How it runs a task

```
discover runner → bootstrap env → cached baseline → reproduce →
  [agent fix ⇄ concentric-ring verify] → accept (extract patch) | reject (rollback)
```

- **Runner discovery** is heuristic and probe-confirmed: a CI-config oracle
  (extracting the project's real install commands) is tried first, then
  pytest/go/rust detectors — each only trusted once it enumerates tests. The
  heuristics live in Rhai (`detectors/*.rhai`); the trust gate is Rust.
- **Concentric rings** keep cost bounded: refinement verifies only the targets;
  the full regression suite runs at most once, after a flip.
- **Bench isolation is structural**: the agent runs with the base + code-graph
  tools and *no* extension host, so the task repo's own `.pirs`/hooks/MCP never
  load.

## CLI

```bash
# One instance (repo already checked out at the base commit):
ANTHROPIC_API_KEY=… pirs-bench solve ./repo \
  -t "pkg/test_mod.py::test_add" -k "pkg/test_mod.py::test_sub" \
  --issue-file bug.md --out fix.patch

# A whole dataset (JSONL: {id, repo, targets, keep_green, issue, base_sha}):
ANTHROPIC_API_KEY=… pirs-bench batch instances.jsonl --out-dir patches/

# Self-check: generate small buggy projects and run the harness over them.
pirs-bench selftest --count 50                 # deterministic oracle fix
DEEPSEEK_API_KEY=… pirs-bench selftest --count 50 --agent \
  --provider deepseek --model deepseek-v4-flash
```

### Providers

`--provider anthropic` (default, `ANTHROPIC_API_KEY`) or `--provider deepseek`
(`DEEPSEEK_API_KEY`, OpenAI-compatible endpoint). Any OpenAI-compatible backend
works via `Provider::OpenAiCompat`.

### Token accounting

Every session reports per-model token usage (input / cache-read / cache-write /
output / reasoning) and behavior stats (turns, tool calls). `batch` and
`selftest` also print the aggregate across all instances.

### Timing

Every run reports where the wall-clock went, broken into non-overlapping phases
(`discover`, `bootstrap`, `baseline`, `fix`, `verify`, `patch`) with each phase's
share of the total; `fix`/`verify` roll up with an `n=` count across retries. In
`solve` mode the `fix` phase is further split per-tool (`fix→tools: bash:… edit:…`)
to separate LLM latency from tool execution. `batch` and `selftest` print the
aggregate across all instances.

## Running on SWE-bench Lite

See [`docs/SWE-BENCH-LITE.md`](docs/SWE-BENCH-LITE.md) for the full runbook:
per-instance repo prep (checkout + commit the test patch), the field mapping,
the environment-setup caveats, and how to read the attribution/timing output.

## Validation

`selftest` is the reproducible check. The deterministic oracle validates the
harness pipeline; `--agent` validates the whole thing end to end.

```bash
# Pipeline only (no model, no key) — must be 100%:
pirs-bench selftest --count 50

# End to end with a real model:
DEEPSEEK_API_KEY=… pirs-bench selftest --count 50 --agent \
  --provider deepseek --model deepseek-v4-flash
```

A representative agent run over the 50 generated projects (deepseek-v4-flash):

- **50/50 solved (100%)**, attribution histogram clean.
- **0 test files modified** — every fix is source-only (test-file protection +
  honest tool use); a fix that touched a test would be reverted before the gate
  and could not pass.
- Minimal edits: ~1 `edit` call per project; the agent `read`s the source and
  runs the suite via `bash` before the harness verifies.
- Per-model token totals (input / cache-read / cache-write / output / reasoning)
  reported for every session and in aggregate.

## Crates

- `pirs-bench` — the harness: gate, differential verify, discovery, baseline
  cache, localization, git workspace, attribution. Pure and dependency-light.
- `pirs-bench-runner` — wires the real pirs agent as the fix `Executor`, plus the
  `pirs-bench` CLI and the self-test corpus.
