# shrimpswe × pirs — results

## Honest harness (clean-v2, 2026-07-31)

**Config:** `--cwd` (not `-C`) · `--autonomy full` · restored fixture · models
`deepseek-v4-flash` (+ `deepseek-v4-pro` for plan phases) · `bun:test` grader · own-tests guard.

**Out:** `runs/clean-v2/`

| Arm | multi-site | red-herring | needle | broken-env | moving-target | **Score** |
|-----|------------|-------------|--------|------------|---------------|-----------|
| **weak-drive** | FIXED | FIXED | FIXED | FIXED | NOT-FIXED | **4/5** |
| **plan-exec** | FIXED | FIXED | FIXED | FIXED | NOT-FIXED | **4/5** |
| **naive** (no strategy) | FIXED | NOT-FIXED | FIXED | FIXED | NOT-FIXED | **3/5** |
| **monolithic** | FIXED | FIXED | FIXED | NOT-FIXED | NOT-FIXED | **3/5** |

### Patterns

- **moving-target** (mid-session redesign for streaming/OOM): **0/4 arms**. Hardest task; export works, streaming redesign fails.
- **red-herring**: fails only **naive** (2/4 mutation traps); multi-phase / mono get full 4/4.
- **broken-env**: fails only **monolithic** (14/15); plan phases help dual-gate recovery.
- **multi-site / needle**: all arms FIXED under the honest harness.

### Harness bugs that invalidated earlier runs

Earlier scores (e.g. weak-drive 1/5, naive/mono 0/5) are **not** comparable:

1. Used **`-C`** which is **not** a pirs cwd flag → agent ran in `qa/shrimpswe/` and edited the shared `fixture/`.
2. Default **autonomy=edit** blocked `bash` / tests → thrash + false incompletes.
3. Polluted fixture (h9 “fixed”, `rows.ts` syntax junk) broke later grading.

**Do not cite pre–clean-v2 numbers.**

### How to re-run

```bash
export CARGO_TARGET_DIR=$HOME/hero/build/target
cargo build -p pirs --release
cd qa/shrimpswe
# restore template if needed:
#   chmod -R u+w fixture && rsync -a --delete $HOME/hero/code/hero_shrimp/support/extbench/shrimpswe/fixture/ fixture/
STRATEGY=weak-drive AUTONOMY=full MODEL=deepseek-v4-flash PLAN_MODEL=deepseek-v4-pro ./run.sh
```

## moving-target recheck (guided, 2026-07-31)

After adding skill `streaming-export`, weak-drive/plan-exec streaming prompt lines,
and injecting the three pull-based rules into the moving-target prompt wrapper:

| Run | Strategy | Streaming assertion | CSV ok | Verdict |
|-----|----------|---------------------|--------|---------|
| clean-v2 (baseline) | weak-drive / plan-exec / mono / naive | **fail** (0/4) | pass | NOT-FIXED |
| **guided re-run** | weak-drive + guidance | **pass** | pass | **FIXED** (~748s) |

Evidence: `runs/moving-target-guided/` (and implementer scratch `moving-target-rerun/`).  
Both hidden checks green (`the export still produces correct CSV` and
`it does NOT materialise every row before emitting`); own-tests clean.

**Takeaway:** the miss was API shape (string join), not model unavailability —
explicit pull-based rules in the agent context moved this task to FIXED.
