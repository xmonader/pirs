# SWE-bench QA campaigns

How we run, land, and compare pirs (and peer) agents on SWE-bench Lite in this
repo. For harness internals and one-instance prep, also read
[`crates/pirs-bench/docs/SWE-BENCH-LITE.md`](../crates/pirs-bench/docs/SWE-BENCH-LITE.md)
and [`crates/pirs-bench/README.md`](../crates/pirs-bench/README.md).

## Where things live

```
qa/bench-swebench-5x5/
  LEADERBOARD.md              ← ranked strict / peer scores (read this first)
  instances/                  ← task JSON stubs used by runners
  results_<campaign>/         ← per-campaign artifacts (must land in-repo)
    *.result.json | *.log | *.patch | *.diff | REPORT*.md
  results_deepseek_v4_flash_strict_weak_drive_fifty/
    REPORT.md                 ← combined fifty
    REPORT-campaign.md        ← design, tokens, failures
    REPORT-strategy-comparison.md
  run_*.py / rerun_*.py       ← campaign drivers (historical + reusable)
  land_results.sh             ← helper to copy artifacts into the tree
```

**Rule:** campaigns write under `qa/bench-swebench-5x5/results_*`, not only
`/tmp`. Gitignore is set so logs under this tree can be tracked.

## Protocols (do not mix scores)

| Protocol | Agent sees | Grade | Use for |
|----------|------------|-------|---------|
| **Strict** | Problem + repo; **no** `test_patch` / F2P names in tree | Oracle after | Honest leaderboard |
| Fair | test_patch pre-applied | harness | Easier ceiling |
| RAW ids | F2P names spoon-fed | harness | Localization ablations |
| Shadow-verify | Strict blindness + mid-loop hidden grade | harness | Multi-attempt mono |

Compare **strict** rows to other **strict** rows only. Headline weak-drive
number is strict.

## Leaderboard snapshot (strict Lite-50)

Authoritative table: [`qa/bench-swebench-5x5/LEADERBOARD.md`](../qa/bench-swebench-5x5/LEADERBOARD.md).

| Campaign | Models | Score | Dir |
|----------|--------|------:|-----|
| **pirs strict weak-drive** | **flash + pro** | **34/50** | `results_deepseek_v4_flash_strict_weak_drive_fifty` |
| pirs strict-verify-v2 | flash only | 32/50 | `…_strict_verify_v2_fifty` |
| pirs strict-naive-v2 | flash only | 31/50 | `…_strict_naive_v2_fifty` |
| pirs strict-v2 mono | flash only | 29/50 | `…_strict_v2_fifty` |
| pi strict | flash only | 27/50 | `results_pi_deepseek_v4_flash_strict_fifty` |

Default model column on the leaderboard = **executor**. Only weak-drive lists
a second model (`deepseek-v4-pro` as `--plan-model`).

## weak-drive campaign (measured)

**CLI shape (all slices):**

```bash
pirs-bench solve … \
  --model deepseek-v4-flash \
  --plan-model deepseek-v4-pro \
  --strategy weak-drive
# provider: DeepSeek (DEEPSEEK_API_KEY), OpenAI-compatible endpoint
```

| Slice | Directory | Score |
|-------|-----------|------:|
| Smoke 5 | `results_deepseek_v4_flash_strict_weak_drive_smoke` | 5/5 |
| Next 20 | `…_weak_drive_next20` | 11/20 |
| Rest 25 | `…_weak_drive_rest25` | 18/25 |
| **Fifty** | combined in `…_weak_drive_fifty` | **34/50** |

Aggregate fifty (from campaign report): ~**$5.75**, ~5.4 h agent wall (sum of
elapsed), cache-heavy; strong (pro) ~62% of $; weak (flash) ~38%.

### What reports must include

Every campaign `REPORT.md` should state:

1. **Models** — exact ids + roles (`--model` / `--plan-model`)
2. **Strategy** name and protocol (strict / fair / …)
3. **Score** N/50 (or N/slice)
4. **Cost** and **tokens** (in / cache_read / out / reasoning) — total and by model when dual
5. **Wall** (sum elapsed and/or campaign wall)
6. Failures list + cost outliers

Strategy comparison docs should not hide that mono/naive rows are flash-only
while weak-drive is flash+pro.

## Build the bench binary

```bash
cargo build --release -p pirs-bench-runner
# → target/release/pirs-bench
```

Self-check without models:

```bash
./target/release/pirs-bench selftest --count 50
```

## Running one instance (discipline)

Environment setup is the fragile part. Always:

1. Get **one** instance green end-to-end.
2. Confirm protocol (strict vs fair).
3. Scale to smoke (5) → batch → full fifty.
4. Land artifacts into `results_*` immediately (JSON, logs, patches, REPORT).

Prep semantics (test patch committed for harness paths that need FAIL_TO_PASS
present; strict phase-1 may deliberately withhold it — follow the runner flags
you intend). Details:
[SWE-BENCH-LITE.md](../crates/pirs-bench/docs/SWE-BENCH-LITE.md).

### Agent-only strict phase

`pirs-bench` supports agent-only then grade paths (see
`crates/pirs-bench-runner/src/main.rs` for `--agent-only` / grade flags). When
comparing, keep the same phase protocol as prior campaigns in the same
results directory family.

## Dual-model on the bench

`pirs-bench` accepts `--plan-model` and pins readonly phases via
`pin_plan_model`. Use with multi-phase strategies (`weak-drive`, `plan-exec`,
`spark-ember`, …).

```bash
export DEEPSEEK_API_KEY=…

./target/release/pirs-bench solve /path/to/checkout \
  --provider deepseek \
  --model deepseek-v4-flash \
  --plan-model deepseek-v4-pro \
  --strategy weak-drive \
  --issue-file bug.md \
  -t 'path/test.py::test_case' \
  --out fix.patch
```

Batch/instance JSONL field mapping is documented in the bench Lite guide.

## Updating the leaderboard

After a finished campaign:

1. Ensure `results_<name>/` is complete (results + REPORT with models).
2. Edit `LEADERBOARD.md` — add or update the row; keep strict/fair separated.
3. Optionally refresh `qa/bench-swebench-5x5.md` narrative if the campaign is
   a major protocol change (historical doc; prefer LEADERBOARD + per-dir REPORTs
   as canonical numbers).

## Interpreting results

- **Solved** under strict means the agent patch flips oracle tests without
  seeing the test patch content as a free lunch.
- High cache_read is normal on long trajectories; still report it.
- Cost outliers (e.g. sympy thrash) belong in the report, not only in JSON.
- Product hybrid thrash→advisor may differ from bench phase-only dual-model;
  see [STRATEGIES.md](STRATEGIES.md) “Product vs bench”.

## Related

- Strategy design: [STRATEGIES.md](STRATEGIES.md)
- Hybrid economics research (non-SWE matrices): [hybrid-model-economics.md](hybrid-model-economics.md)
- Agent onboarding: [../AGENTS.md](../AGENTS.md)
