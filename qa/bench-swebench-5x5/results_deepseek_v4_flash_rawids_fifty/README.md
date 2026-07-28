# SWE-bench Lite — deepseek-v4-flash **RAW test IDs** ablation

**Date:** 2026-07-28 (UTC)

**Score: 46/50 (92%)**

## What “RAW IDs” means (and what it is *not*)

This is an **ablation of harness test-id hygiene**, not a gold-patch cheat.

| Mode | What the harness does with `FAIL_TO_PASS` / `PASS_TO_PASS` | Agent still gets target names? |
|------|-----------------------------------------------------------|--------------------------------|
| **Filtered** (`results_deepseek_v4_flash_fifty/`) | Drop non-runnable strings (docstring titles, prose) via `looks_like_test_id`; if all F2P filtered out, recover real `test_*` names from the **test_patch** | **Yes** — listed in the agent prompt as “Tests that must pass” |
| **RAW IDs** (this dir, `PIRS_RAW_TEST_IDS=1`) | Pass dataset F2P/P2P **as-is** — **no** `looks_like_test_id`, **no** test_patch name recovery | **Yes** — same spoon-feeding |
| **Fair** (`--hide-targets` / `PIRS_FAIR=1`) | Same filtered hygiene for **grading only** | **No** — agent sees issue + repo only; harness still verifies hidden F2P/P2P |

### What both filtered and RAW still do (not “cheating gold”)

- Apply **only** the official `test_patch` (so new FAIL_TO_PASS tests exist). Never apply gold `patch`.
- Give the agent the public `problem_statement`.
- Grade: FAIL_TO_PASS red→green + keep-green; agent edits to test files are restored.

### Why RAW scored higher than filtered (46 vs 42)

`looks_like_test_id` is **hygiene**, not gold leakage — but it can **drop or rewrite** runnable selectors when the dataset stores odd id shapes. RAW keeps the dataset strings untouched, so some instances that failed under the filter (wrong/empty target set → early `ReproFailed` or bad focus) can solve under RAW.

That means the 42/50 filtered score was partly **under-counting** solvable work when ids were over-filtered — not that RAW invents answers from gold.

Conversely, both filtered and RAW **still hand target test names to the agent**. High absolute scores (84–92%) are inflated vs a true issue-only setup. Use the **fair** campaign for that.

## Setup

- Model: `deepseek-v4-flash` via DeepSeek API
- Strategy: `monolithic`
- Label: `deepseek-v4-flash-rawids`
- Concurrency: 3
- Env: `PIRS_RAW_TEST_IDS=1`
- Binary: musl `pirs-bench` (`pirs-bench-runner`)

## Token usage

| Metric | Aggregate |
|---|---:|
| **input (in)** | 726,255 |
| **cache read (cache_r)** | 24,713,088 |
| **cache write (cache_w)** | 0 |
| **output (out)** | 395,069 |
| **reasoning** | 242,948 |
| **total (reported)** | 25,834,412 |
| **cost sum** | $2.3605 (50 instances) |

Per-instance: [`tokens_summary.json`](tokens_summary.json).

## Failures (4)

- `django__django-11283`
- `django__django-11797`
- `django__django-15695`
- `sympy__sympy-16106`

## Head-to-head with filtered (same 50 instances, same model)

| | Filtered | RAW IDs |
|---|---:|---:|
| Score | **42/50 (84%)** | **46/50 (92%)** |
| Delta | — | **+4** |

Instances that **failed filtered but solved RAW** (illustrative of filter harm):  
`django-11019`, `django-11133`, `django-11742`, `django-12125`, `django-15213`, plus others depending on exact run — see per-instance `*.result.json` in both dirs.

Instances that **solved filtered but failed RAW**: e.g. `django-11283`, `django-15695`, `sympy-16106` (noise / different agent trajectories).

## Artifacts

- `*.result.json` — outcome + `raw_test_ids: true` + `tokens`
- `*.deepseek-v4-flash-rawids.log` / `.patch`
- `SCORE.txt`, `REPORT.md`, `summary.json`, `tokens_summary.json`
