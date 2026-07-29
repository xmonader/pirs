# SWE-bench Lite (50) — deepseek-v4-flash leaderboard

Same instance set. **Strict** = agent never sees `test_patch` / F2P names;
grade after with oracle tests. **Shadow-verify** = same blindness + mid-loop
hidden grading.

| Campaign | Dir | Score | Notes |
|----------|-----|------:|-------|
| pirs strict-naive-v2 | `results_deepseek_v4_flash_strict_naive_v2_fifty` | **31/50** | export fix, `--no-strategy` |
| pirs strict-v2 mono | `results_deepseek_v4_flash_strict_v2_fifty` | **29/50** | export fix, monolithic |
| pi strict | `results_pi_deepseek_v4_flash_strict_fifty` | **27/50** | same harsh exam |
| pirs strict-verify-v2 | `results_deepseek_v4_flash_strict_verify_v2_fifty` | (see dir) | hidden multi-attempt loop |
| pirs strict v1 | `results_deepseek_v4_flash_strict_fifty` | **20/50** | pre-export-fix (corrupt patches) |
| pirs fair | `results_deepseek_v4_flash_fair_fifty` | 45/50 | test_patch pre-applied |
| pirs RAW ids | `results_deepseek_v4_flash_rawids_fifty` | 46/50 | F2P names spoon-fed |
| pirs filtered | `results_deepseek_v4_flash_fifty` | 42/50 | earlier protocol |

## Always land results in-repo

Campaigns must write under `qa/bench-swebench-5x5/results_*` (not only `/tmp`).
`.gitignore` un-ignores **all** logs under this tree so new dirs are tracked
without further edits.
