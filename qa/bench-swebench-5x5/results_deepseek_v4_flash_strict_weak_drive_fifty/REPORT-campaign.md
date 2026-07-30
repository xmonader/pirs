# weak-drive campaign report

*Generated 2026-07-30. Protocol: **strict** (agent never sees test_patch / F2P names).*

## Models (pinned for all weak-drive runs)

| Role | Model ID | CLI flag | Phases / work |
|---|---|---|---|
| **Weak executor** | **`deepseek-v4-flash`** | `--model` | `#1` exec (full tools), `#3` fixup (full tools) |
| **Strong planner / reviewer** | **`deepseek-v4-pro`** | `--plan-model` | `#0` plan (readonly), `#2` review (readonly) |
| API | DeepSeek OpenAI-compatible | `--provider deepseek` | Same base URL for both |

Confirmed in every result JSON: `"model": "deepseek-v4-flash"`, `"plan_model": "deepseek-v4-pro"`.

Token accounting by model (from logs) is under **Weak vs strong model split** below.

## What we built

### Strategy `weak-drive` (agentpy hybrid ideas in pirs)

| Piece | Implementation |
|---|---|
| Scheduled phases | strong plan → weak exec → strong review → weak fixup |
| Multi-model | `--model deepseek-v4-flash` (full), `--plan-model deepseek-v4-pro` (readonly) via `pin_plan_model` |
| Plan retention | `{prev_0}` so review still sees the original plan |
| Free APPROVE | `skip_if_prev_prefix: APPROVE` skips fixup |
| Hybrid flag | `hybrid: true` — thrash→advisor + `ask_advisor` on **product** `pirs` CLI (advisor = plan-model) |
| Bench path | Same 4 phases; mid-loop thrash escalate is product-path only |
| Aliases | `advisor`, `weak-strong`, `advise-exec` |

### Campaigns run (weak-drive only)

All slices: **flash + pro**, strategy `weak-drive`, strict.

| Slice | Directory | Score | Models |
|---|---|---:|---|
| Smoke 5 | `results_deepseek_v4_flash_strict_weak_drive_smoke` | **5/5** | flash + pro |
| Next 20 | `results_deepseek_v4_flash_strict_weak_drive_next20` | **11/20** | flash + pro |
| Rest 25 | `results_deepseek_v4_flash_strict_weak_drive_rest25` | **18/25** | flash + pro |
| **Fifty** | combined here | **34/50 (68%)** | flash + pro |

## Aggregate tokens & cost (fifty)

| Metric | Value |
|---|---:|
| Score | **34/50** |
| Cost | **$5.750** |
| Fresh in (miss) | 1,991,245 |
| Cache read | 58,314,624 |
| Out | 1,027,034 |
| Reasoning | 641,645 |
| Total (incl. cache) | 61,332,903 |
| Agent wall (sum elapsed) | 5.4 h |
| Cache hit rate | 96.7% |
| $/instance | $0.1150 |
| $/solved | $0.1691 |

### Weak vs strong model split

From each log’s `tokens by model:` block (50/50 present).

| Role | Model | in | cache_r | out | reasoning | total | cost | share $ |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| **Weak** (exec/fixup) | deepseek-v4-flash | 709,849 | 23,272,192 | 340,046 | 154,034 | 24,322,087 | **$2.194** | 38% |
| **Strong** (plan/review) | deepseek-v4-pro | 1,281,396 | 35,042,432 | 686,988 | 487,611 | 37,010,816 | **$3.555** | 62% |

Strong dominates **cost** and **reasoning** because plan+review run on pro; flash owns tool-loop work at lower $/token.

### Slice breakdown

- **smoke**: 5/5 · $0.348 · in=163,527 · cache_r=3,174,144 · out=74,254 · reasoning=43,855
- **next20**: 11/20 · $2.289 · in=818,075 · cache_r=23,031,168 · out=414,508 · reasoning=264,340
- **rest25**: 18/25 · $3.112 · in=1,009,643 · cache_r=32,109,312 · out=538,272 · reasoning=333,450

### Failures (16)

- `astropy__astropy-14182`
- `astropy__astropy-14365`
- `astropy__astropy-7746`
- `django__django-10914`
- `django__django-11019`
- `django__django-11564`
- `django__django-11630`
- `django__django-11905`
- `django__django-11964`
- `django__django-15695`
- `django__django-15781`
- `pytest-dev__pytest-5221`
- `sphinx-doc__sphinx-7686`
- `sympy__sympy-13177`
- `sympy__sympy-13647`
- `sympy__sympy-16106`

### Cost outliers (top 5)

| Instance | solved | cost | in | cache_r | out | weak$ | strong$ |
|---|---|---:|---:|---:|---:|---:|---:|
| `sympy__sympy-15346` | True | $0.693 | 167,878 | 7,499,648 | 111,623 | $0.283 | $0.410 |
| `django__django-11797` | True | $0.584 | 165,528 | 6,455,936 | 79,095 | $0.313 | $0.271 |
| `django__django-11019` | False | $0.449 | 120,930 | 4,320,896 | 103,643 | $0.236 | $0.213 |
| `django__django-12113` | True | $0.268 | 92,925 | 2,794,624 | 42,656 | $0.115 | $0.153 |
| `sympy__sympy-16106` | False | $0.262 | 66,273 | 2,956,800 | 33,886 | $0.137 | $0.125 |

### Local pre-SWE smoke

Product CLI: sliding-window rate limiter with `pirs --strategy weak-drive --model flash --plan-model pro`. Gate passed attempt 1 (5/5 pytest). Hybrid banner and all four phases observed.

### Related reports

- [Strategy comparison (50)](./REPORT-strategy-comparison.md)
- [Per-instance summary](./REPORT.md)
