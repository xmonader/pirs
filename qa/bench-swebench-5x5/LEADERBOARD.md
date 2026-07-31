# SWE-bench Lite (50) — deepseek-v4-flash leaderboard

Same instance set. **Strict** = agent never sees `test_patch` / F2P names;
grade after with oracle tests. **Shadow-verify** = same blindness + mid-loop
hidden grading.

**Default model column = executor.** Only **weak-drive** also uses a second model
(`deepseek-v4-pro` as `--plan-model` for readonly plan/review).

## Honesty notes (read before citing scores)

1. **PASS_TO_PASS sample.** Harness keep-green lists are capped at
   `PIRS_MAX_KEEP_GREEN` (default **40**). Official SWE-bench oracle may run a
   larger P2P set; scores here are harness-relative regression samples, not a
   full-suite claim. Set `PIRS_MAX_KEEP_GREEN=0` to disable the cap (slower).
2. **Strict oracle scrub (current harness).** Plain `PIRS_STRICT=1` defers
   `/tmp/test.patch` until after the agent, and prunes git remotes/tags/reflog
   so the agent cannot `cat` grade tests or `git show` a fetched gold commit.
   Historical 34/50 weak-drive results predate this scrub; re-run before
   treating them as fully issue-only.
3. **strict_verify residual.** Shadow mode still mounts `test_patch` for the
   mid-loop grader; a shellful agent *could* read that path. Prefer plain
   strict for issue-only claims.
4. **Git history.** Images usually check out base only; scrub removes remotes
   but cannot invent absence of commits already in the local object store.
   Report residual risk if an image ships extra history.

| Campaign | Models | Dir | Score | Notes |
|----------|--------|-----|------:|-------|
| **pirs strict weak-drive** | **flash + pro** | `results_deepseek_v4_flash_strict_weak_drive_fifty` | **34/50** | `--model deepseek-v4-flash --plan-model deepseek-v4-pro --strategy weak-drive`; P2P cap 40; pre-scrub campaign — see honesty notes |
| pirs strict-verify-v2 | flash only | `results_deepseek_v4_flash_strict_verify_v2_fifty` | **32/50** | mono + hidden multi-attempt loop |
| pirs strict-naive-v2 | flash only | `results_deepseek_v4_flash_strict_naive_v2_fifty` | **31/50** | export fix, `--no-strategy` |
| pirs strict-v2 mono | flash only | `results_deepseek_v4_flash_strict_v2_fifty` | **29/50** | export fix, monolithic |
| pi strict | flash only | `results_pi_deepseek_v4_flash_strict_fifty` | **27/50** | same harsh exam |
| pirs strict spark-ember smoke | flash only | `results_deepseek_v4_flash_strict_spark_ember_smoke` | **4/5** | dual-mode; no plan-model in results |
| pirs strict spark-ember next20 | flash only | `results_deepseek_v4_flash_strict_spark_ember_next20` | **9/20** | dual-mode batch after smoke |
| pirs strict v1 | flash only | `results_deepseek_v4_flash_strict_fifty` | **20/50** | pre-export-fix (corrupt patches) |
| pirs fair | flash only | `results_deepseek_v4_flash_fair_fifty` | 45/50 | test_patch pre-applied |
| pirs RAW ids | flash only | `results_deepseek_v4_flash_rawids_fifty` | 46/50 | F2P names spoon-fed |
| pirs filtered | flash only | `results_deepseek_v4_flash_fifty` | 42/50 | earlier protocol |

## Always land results in-repo

Campaigns must write under `qa/bench-swebench-5x5/results_*` (not only `/tmp`).
`.gitignore` un-ignores **all** logs under this tree so new dirs are tracked
without further edits.
