# SWE-bench Lite — deepseek-v4-flash **STRICT** (no test_patch for agent)

**Date:** 2026-07-28/29 (UTC)

**Score: 20/50 (40%)**

## What this mode is

| Mode | Agent sees F2P names? | `test_patch` before agent? | Score |
|------|----------------------|----------------------------|------:|
| [Filtered](../results_deepseek_v4_flash_fifty/) | Yes | Yes | 42/50 |
| [RAW IDs](../results_deepseek_v4_flash_rawids_fifty/) | Yes | Yes | 46/50 |
| [Fair](../results_deepseek_v4_flash_fair_fifty/) | No | Yes | 45/50 |
| **This dir (strict)** | **No** | **No** | **20/50** |

**Strict flow (`PIRS_STRICT=1`):**

1. Agent runs on **base commit only** with issue text (`--agent-only --hide-targets`)
2. Tree is reset; **`test_patch` applied only for grading**
3. Model patch graded with `--check-patch` (F2P red→green + keep-green)

No gold patch. No FAIL_TO_PASS names in the prompt. No pre-applied tests for the agent to read.

This is the honest drop from “tests in tree” (fair 90%) → issue-only (strict **40%**).

## Failures (30)

Many fails are real miss / incomplete fix. Some are **`git apply` reject** on the model patch after `test_patch` is applied (`corrupt patch` / `FixNoFlip`) — agent edited lines that conflict with the later test_patch or produced a non-applyable diff.

## Token usage

| Metric | Aggregate |
|---|---:|
| **input (in)** | 1,038,338 |
| **cache read (cache_r)** | 39,367,424 |
| **output (out)** | 525,027 |
| **reasoning** | 303,923 |
| **total (reported)** | 40,930,789 |
| **cost sum** | $3.6135 (50 instances) |

Per-instance: [`tokens_summary.json`](tokens_summary.json).

## Setup

- Model: `deepseek-v4-flash`
- Strategy: `monolithic`
- Label: `deepseek-v4-flash-strict`
- Concurrency: 3
- Binary: musl `pirs-bench` with `--agent-only` / `--check-patch`
