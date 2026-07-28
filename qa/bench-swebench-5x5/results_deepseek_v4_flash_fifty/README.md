# SWE-bench Lite — deepseek-v4-flash campaign (**filtered** test ids)

**Date:** 2026-07-28 (UTC)

**Score: 42/50 (84%)**

## How this differs from RAW / Fair

| Mode | Agent sees F2P names? | Id hygiene | Score (same 50) |
|------|----------------------|------------|-----------------|
| **This dir (filtered)** | Yes | `looks_like_test_id` + test_patch recovery | **42/50** |
| [RAW IDs](../results_deepseek_v4_flash_rawids_fifty/) | Yes | none (`PIRS_RAW_TEST_IDS=1`) | **46/50** |
| Fair (`PIRS_FAIR=1` / `--hide-targets`) | **No** | filtered for grading only | (running / TBD) |

**Filtered is not gold cheating.** It still applies only `test_patch` and never gold `patch`.  
**But it does spoon-feed** FAIL_TO_PASS ids into the agent prompt (same as RAW). The filter only drops docstring/prose “ids” that are not runnable.

See [rawids README](../results_deepseek_v4_flash_rawids_fifty/README.md) for why RAW can score higher (filter under-count, not gold).

## Setup (honest / no gold-patch cheat)

- Model: `deepseek-v4-flash` via DeepSeek API (`--provider=deepseek`)
- Strategy: `monolithic`
- Harness: `pirs-bench solve` inside official `swebench/sweb.eval.*` images
- Runner: `qa/bench-swebench-5x5/run_one.py`
- Applies **only** `test_patch` (not gold `patch`); agent gets `problem_statement`
- **Agent also gets FAIL_TO_PASS names** (filtered) as “tests that must pass”
- Success = FAIL_TO_PASS red→green + keep-green; test-file edits reverted
- First 8 sequential, then remaining 42 with **concurrency 3**
- Binary: `pirs-bench` musl release (`pirs-bench-runner`)

## Summary

| Metric | Value |
|---|---|
| Solved | 42 |
| Failed | 8 |
| Total wall-clock (sum of instance elapsed) | 8008s |


## Token usage (from pirs-bench session stats)

Parsed from each run's `stderr_tail` TOTAL line (`in` / `cache_r` / `cache_w` / `out` / `reasoning` / `total`).

| Metric | Aggregate |
|---|---:|
| **input (in)** | 838,945 |
| **cache read (cache_r)** | 29,740,672 |
| **cache write (cache_w)** | 0 |
| **output (out)** | 400,484 |
| **reasoning** | 228,786 |
| **total (reported)** | 30,980,101 |
| **cost sum (priced models)** | $2.7489 (50 instances with $) |

Note: `total` from the harness includes cache-read tokens in its accounting; do not sum in+out+reasoning alone if you want the same total.

Per-instance breakdown: [`tokens_summary.json`](tokens_summary.json). Structured `tokens` object also added to each `*.result.json`.

## Failures

- `django__django-11019` (t=67.9s, exit=1)
- `django__django-11133` (t=10.2s, exit=1)
- `django__django-11742` (t=59.2s, exit=1)
- `django__django-11797` (t=429.3s, exit=1)
- `django__django-12125` (t=51.1s, exit=1)
- `django__django-15213` (t=104.3s, exit=1)
- `sympy__sympy-13647` (t=206.6s, exit=1)
- `sympy__sympy-15346` (t=163.7s, exit=1)

## Per-instance results

| Instance | Solved | Time (s) | Patch bytes | Exit |
|---|---|---|---|---|
| `astropy__astropy-12907` | True | 502.6 | 502 | 0 |
| `astropy__astropy-14182` | True | 562.0 | 963 | 0 |
| `astropy__astropy-14365` | True | 230.0 | 997 | 0 |
| `astropy__astropy-14995` | True | 227.1 | 669 | 0 |
| `astropy__astropy-6938` | True | 103.2 | 528 | 0 |
| `astropy__astropy-7746` | True | 219.1 | 1117 | 0 |
| `django__django-10914` | True | 159.3 | 625 | 0 |
| `django__django-10924` | True | 53.4 | 1013 | 0 |
| `django__django-11001` | True | 48.9 | 1336 | 0 |
| `django__django-11019` | False | 67.9 | None | 1 |
| `django__django-11039` | True | 40.7 | 658 | 0 |
| `django__django-11049` | True | 54.6 | 564 | 0 |
| `django__django-11099` | True | 44.2 | 901 | 0 |
| `django__django-11133` | False | 10.2 | None | 1 |
| `django__django-11179` | True | 38.6 | 614 | 0 |
| `django__django-11283` | True | 94.0 | 1925 | 0 |
| `django__django-11422` | True | 80.0 | 839 | 0 |
| `django__django-11564` | True | 178.0 | 1379 | 0 |
| `django__django-11583` | True | 30.5 | 563 | 0 |
| `django__django-11620` | True | 87.8 | 719 | 0 |
| `django__django-11630` | True | 37.4 | 2182 | 0 |
| `django__django-11742` | False | 59.2 | None | 1 |
| `django__django-11797` | False | 429.3 | None | 1 |
| `django__django-11815` | True | 47.6 | 761 | 0 |
| `django__django-11848` | True | 62.9 | 618 | 0 |
| `django__django-11905` | True | 102.3 | 1138 | 0 |
| `django__django-11910` | True | 208.6 | 1824 | 0 |
| `django__django-11964` | True | 111.4 | 430 | 0 |
| `django__django-11999` | True | 87.3 | 825 | 0 |
| `django__django-12113` | True | 84.4 | 525 | 0 |
| `django__django-12125` | False | 51.1 | None | 1 |
| `django__django-12184` | True | 73.7 | 608 | 0 |
| `django__django-12284` | True | 95.5 | 659 | 0 |
| `django__django-12286` | True | 36.5 | 794 | 0 |
| `django__django-14608` | True | 70.2 | 861 | 0 |
| `django__django-15213` | False | 104.3 | None | 1 |
| `django__django-15695` | True | 172.9 | 832 | 0 |
| `django__django-15781` | True | 185.4 | 1083 | 0 |
| `django__django-16046` | True | 25.6 | 454 | 0 |
| `matplotlib__matplotlib-23562` | True | 569.0 | 553 | 0 |
| `matplotlib__matplotlib-26011` | True | 666.7 | 568 | 0 |
| `pytest-dev__pytest-5221` | True | 143.6 | 961 | 0 |
| `scikit-learn__scikit-learn-12471` | True | 129.5 | 1007 | 0 |
| `scikit-learn__scikit-learn-25570` | True | 534.3 | 2322 | 0 |
| `sphinx-doc__sphinx-7686` | True | 257.3 | 1768 | 0 |
| `sympy__sympy-13177` | True | 160.3 | 3670 | 0 |
| `sympy__sympy-13647` | False | 206.6 | None | 1 |
| `sympy__sympy-15346` | False | 163.7 | None | 1 |
| `sympy__sympy-15678` | True | 145.1 | 4295 | 0 |
| `sympy__sympy-16106` | True | 153.8 | 4573 | 0 |

## Artifacts in this directory

- `*.result.json` — harness outcome per instance
- `*.deepseek-v4-flash.log` — container run log (setup + pirs-bench stdout/stderr tail)
- `*.deepseek-v4-flash.patch` — agent patch when produced
- `summary_all.json` / `parallel_summary.json` — aggregates
- `SCORE.txt` — final `solved/total`
