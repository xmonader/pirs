# QA — feature verification with proof

Every claim below is backed by a captured artifact in this directory. Live runs
were executed against DeepSeek (`--provider openai --base-url
https://api.deepseek.com`) using `deepseek-v4-flash` / `deepseek-v4-pro`. All
logs were scrubbed of API keys before saving (`--api-key <redacted>` in place of
the real secret); no keys or secrets are committed.

## Static gates

| Gate | Proof | Result |
|------|-------|--------|
| Full test suite | `test-suite.txt` | **491 passed, 0 failed** |
| Formatting | `fmt.txt` | clean (`cargo fmt --check`, exit 0) |
| Lint | `clippy.txt` | clean (`clippy -D warnings`, exit 0) |
| CLI surface | `cli-help.txt` | `--help` renders all flags |

Reproduce:

```sh
cargo test --workspace            # -> test-suite.txt
cargo fmt --all --check           # -> fmt.txt
cargo clippy --workspace --all-targets -- -D warnings   # -> clippy.txt
```

## Live feature runs

Each log is a full transcript of the product agent (`pirs`) exercising one
feature end to end against a real model.

| # | Feature | Proof | What it demonstrates |
|---|---------|-------|----------------------|
| 1 | Naive coding loop (pi mode) | `live/01-naive-loop.log` | The agent runs a task with no strategy — plain tool-use loop to completion. |
| 2 | `--strategy plan-exec` | `live/02-strategy-plan-exec.log` | Read-only plan phase → full-scope exec phase, `{prev}` threaded between them. |
| 3 | Oracle routing (`plan-oracle-exec`) | `live/03-oracle-routing.log` | Per-phase model override: only the critic phase runs on the stronger model. |
| 4 | Wide fan-out (`general-wide-plan-exec`) | `live/04-wide-fanout.log` | Three parallel read-only planners merge under `## Branch N` for one executor. |
| 5 | `--profile security-reviewer` | `live/05-profile-security-reviewer.log` | Provider-agnostic profile drives a review pass (no hard-pinned model). |
| 6 | Verify-and-retry gate (pass) | `live/06-gate-pass.log` | `--verify` command runs; passing verify completes the run cleanly. |
| 7 | Verify gate (fail → exit 1) | `live/07-gate-fail-exit1.log` | Exhausted attempts with a failing `--verify` exits non-zero (real exit 1). |

## Discovery

| Feature | Proof | What it demonstrates |
|---------|-------|----------------------|
| Strategy name resolution | `strategy-discovery.txt` | Built-ins resolve by name; unknown names fail with a helpful error. |

## Notes

- Built-in strategies (`monolithic`, `plan-exec`, `plan-critic-exec`,
  `wide-plan-exec`) are embedded in `pirs-rhai`; project `.pirs/strategies/`
  overrides and additions (e.g. `plan-oracle-exec`, `general-*`) resolve by name
  with project-then-home precedence.
- The extension packs are cataloged in `../extensions/README.md` and loaded by
  the `pirs-rhai` integration tests, counted in the 491 above.
