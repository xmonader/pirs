# SWE-bench Lite — deepseek-v4-flash **FAIR** (hide F2P names)

**Date:** 2026-07-28 (UTC)

**Score: 45/50 (90%)**

## What this mode is

| Mode | Agent sees F2P names? | `test_patch` in tree before agent? | Score |
|------|----------------------|-------------------------------------|------:|
| [Filtered](../results_deepseek_v4_flash_fifty/) | Yes | Yes | 42/50 |
| [RAW IDs](../results_deepseek_v4_flash_rawids_fifty/) | Yes | Yes | 46/50 |
| **This dir (fair)** | **No** (`--hide-targets`) | **Yes** | **45/50** |
| [Strict](../results_deepseek_v4_flash_strict_fifty/) (`PIRS_STRICT=1`) | No | **No** (only at grade time) | **20/50** |

**Fair is not gold-patch cheating.** It still applies only `test_patch` (never gold `patch`).  
It **does** hide FAIL_TO_PASS ids from the agent prompt.

**Fair is still easier than official SWE-bench Lite issue-only**, because `test_patch` is committed **before** the agent runs — so the agent can read the new tests / `git show HEAD` and run them. See the comparison docs under rawids README for the full honesty ladder.

## Setup

- Model: `deepseek-v4-flash`
- Strategy: `monolithic`
- Label: `deepseek-v4-flash-fair`
- Concurrency: 3
- Flags: `--hide-targets` / `PIRS_FAIR=1`
- Binary: musl `pirs-bench`

## Failures (5)

- `django__django-12286`
- `django__django-16046`
- `scikit-learn__scikit-learn-25570`
- `sphinx-doc__sphinx-7686`
- `sympy__sympy-13647`

## Token usage

| Metric | Aggregate |
|---|---:|
| **input (in)** | 1,002,826 |
| **cache read (cache_r)** | 44,231,936 |
| **output (out)** | 592,027 |
| **reasoning** | 381,610 |
| **total (reported)** | 45,826,789 |
| **cost sum** | $4.0185 (50 instances) |

Per-instance: [`tokens_summary.json`](tokens_summary.json).

## Artifacts

- `*.result.json` — `hide_targets: true`, `fair: true`, `tokens`
- `*.deepseek-v4-flash-fair.log` / `.patch`
- `SCORE.txt`, `REPORT.md`, `summary.json`
