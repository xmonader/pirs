# SWE-bench Lite — deepseek-v4-flash STRICT v2 (monolithic)

**Score: 29/50 (58%)**

## Protocol

- **Strict agent-only**: agent on base commit, issue text only  
- **No** `test_patch` in workspace, **no** FAIL_TO_PASS names  
- After agent exits: apply `test_patch`, grade with `--check-patch`  
- Strategy: **monolithic**  
- Binary includes export fixes (no corrupt `trim` patches; scrub test-file hunks)

## Comparison

| Run | Score |
|-----|------:|
| Old strict (v1, pre-export-fix) | 20/50 |
| **This run (strict-v2 mono)** | **29/50** |
| Strict-naive-v2 (same protocol, `--no-strategy`) | 31/50 |
| pi strict (same exam) | 27/50 |

## Artifacts

- `*.result.json` — per-instance outcome + tokens  
- `*.log` — full run log  
- `*.patch` — model patch  
- `SCORE.txt`, `summary.json`, `queue.txt`, `nohup.out`, `runner.log`
