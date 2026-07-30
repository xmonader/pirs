# Strategy comparison — SWE-bench Lite 50 (strict)

*Generated 2026-07-30.*

## Setup

| Knob | Value |
|---|---|
| Benchmark | SWE-bench Lite, fixed 50-instance set |
| Protocol | **Strict**: base commit only; no test_patch / F2P in agent view; grade after |
| Harness | `pirs-bench` (or **pi** for the pi arm) in official SWE docker images |
| Provider (pirs arms) | DeepSeek API |

### Models by strategy arm (from result JSON)

| Strategy arm | Primary / exec model | Plan model | Notes |
|---|---|---|---|
| **weak-drive** | **`deepseek-v4-flash`** | **`deepseek-v4-pro`** | Dual-model: pro on readonly plan+review; flash on full exec+fixup |
| **naive** (`--no-strategy`) | **`deepseek-v4-flash`** | *(none)* | Single model for entire undivided loop |
| **monolithic** | **`deepseek-v4-flash`** | *(none)* | Single model, one persistent full phase |
| **shadow-verify** | **`deepseek-v4-flash`** | *(none)* | Same mono strategy; hidden multi-attempt verify (not a second model) |
| **pi** (external) | **`deepseek-v4-flash`** | *(none)* | External agent baseline; same model id in results |
| **spark-ember** (25 only) | **`deepseek-v4-flash`** | *(none in results)* | Both SPARK (readonly) and EMBER (full) ran as flash; no `--plan-model` recorded |

**Important:** Only **weak-drive** on this leaderboard uses a **stronger second model** (`deepseek-v4-pro`). All other listed arms are **flash-only**. Comparing solve rate is fair on the *executor* model; comparing cost is not—weak-drive intentionally buys pro plan/review tokens.

## Headline scores

| Strategy | Models | Score | Cost | Fresh in | Cache read | Out | Reasoning | Total | Wall |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **weak-drive** | **flash + pro** | **34/50** | $5.750 | 1,991,245 | 58,314,624 | 1,027,034 | 641,645 | 61,332,903 | 5.4h |
| shadow-verify | flash only | **32/50** | $7.776 | 1,326,903 | 91,646,848 | 911,396 | 570,500 | 93,885,147 | 6.1h |
| naive (`--no-strategy`) | flash only | **31/50** | $3.280 | 886,355 | 35,516,672 | 504,365 | 287,967 | 36,907,392 | 2.8h |
| monolithic | flash only | **29/50** | $3.938 | 914,204 | 43,618,432 | 579,764 | 336,998 | 45,112,400 | 3.8h |
| pi (external) | flash only | **27/50** | $0.000* | 0* | 0* | 0* | 0* | 0* | 3.1h |
| spark-ember (25 only) | flash only | **13/25** | $2.950 | 1,182,594 | 28,862,720 | 554,602 | 318,956 | 30,599,916 | 1.7h |

\* pi results did not record per-token `$` in the same `tokens` schema (elapsed only).

### Solve rate chart (strict fifty)

```
weak-drive     34/50 (  68%)  ██████████████████████████████████░░░░░░░░░░░░░░░░  $5.75
shadow-verify  32/50 (  64%)  ████████████████████████████████░░░░░░░░░░░░░░░░░░  $7.78
naive          31/50 (  62%)  ███████████████████████████████░░░░░░░░░░░░░░░░░░░  $3.28
monolithic     29/50 (  58%)  █████████████████████████████░░░░░░░░░░░░░░░░░░░░░  $3.94
pi             27/50 (  54%)  ███████████████████████████░░░░░░░░░░░░░░░░░░░░░░░  $0.00
```

## Efficiency

| Strategy | $/solved | $/instance | solved |
|---|---:|---:|---:|
| naive | $0.106 | $0.066 | 31 |
| monolithic | $0.136 | $0.079 | 29 |
| shadow-verify | $0.243 | $0.156 | 32 |
| weak-drive | $0.169 | $0.115 | 34 |
| pi | $0.000 | $0.000 | 27 |

weak-drive is the **most expensive** arm and the **highest score**. You pay for pro plan+review on every task; cache hits keep marginal cost from exploding.

## weak-drive: weak vs strong spend

Models: **weak = `deepseek-v4-flash`**, **strong = `deepseek-v4-pro`**.

| | Weak `deepseek-v4-flash` | Strong `deepseek-v4-pro` |
|---|---:|---:|
| Cost | $2.194 (38%) | $3.555 (62%) |
| Fresh in | 709,849 | 1,281,396 |
| Cache read | 23,272,192 | 35,042,432 |
| Out | 340,046 | 686,988 |
| Reasoning | 154,034 | 487,611 |

Plan+review (pro) dominate **reasoning** and **$**; flash does tool volume cheaper. Bench does **not** currently bill mid-loop thrash→advisor (product-only).

## Pairwise solve overlap (n=50)

### mono vs weak-drive

| | weak-drive ✓ | weak-drive ✗ |
|---|---:|---:|
| **mono ✓** | 28 | 1 |
| **mono ✗** | 6 | 15 |

Net: weak-drive recovers **6** that mono misses; loses **1** that mono gets.
- mono-only: `django__django-11964`
- weak-drive-only: `astropy__astropy-6938`, `django__django-11283`, `django__django-11742`, `django__django-11848`, `django__django-11910`, `sympy__sympy-15678`

### naive vs weak-drive

| | weak-drive ✓ | weak-drive ✗ |
|---|---:|---:|
| **naive ✓** | 30 | 1 |
| **naive ✗** | 4 | 15 |

Net: weak-drive recovers **4** that naive misses; loses **1** that naive gets.
- naive-only: `django__django-11964`
- weak-drive-only: `django__django-11283`, `django__django-11742`, `django__django-12113`, `sympy__sympy-15678`

### shadow vs weak-drive

| | weak-drive ✓ | weak-drive ✗ |
|---|---:|---:|
| **shadow ✓** | 30 | 2 |
| **shadow ✗** | 4 | 14 |

Net: weak-drive recovers **4** that shadow misses; loses **2** that shadow gets.
- shadow-only: `astropy__astropy-14365`, `django__django-11630`
- weak-drive-only: `django__django-11742`, `django__django-11797`, `django__django-11848`, `sympy__sympy-15346`

### naive vs mono

| | mono ✓ | mono ✗ |
|---|---:|---:|
| **naive ✓** | 28 | 3 |
| **naive ✗** | 1 | 18 |

Net: mono recovers **1** that naive misses; loses **3** that naive gets.
- naive-only: `astropy__astropy-6938`, `django__django-11848`, `django__django-11910`
- mono-only: `django__django-12113`

## spark-ember (first 25 only)

| Strategy | Score on those 25 | Cost on 25 |
|---|---:|---:|
| spark-ember | 13/25 | $2.950 |
| weak-drive | 16/25 | $2.637 |
| monolithic | 13/25 | $2.102 |
| naive | 16/25 | $1.631 |

On overlap: both=13, spark-only=0, weak-drive-only=3, neither=9.
weak-drive-only: `django__django-10924`, `django__django-11283`, `django__django-11848`

## Per-instance solve matrix

| Instance | mono | naive | shadow | weak-drive | pi | spark* |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| `astropy__astropy-12907` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `astropy__astropy-14182` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| `astropy__astropy-14365` | ✗ | ✗ | ✓ | ✗ | ✗ | ✗ |
| `astropy__astropy-14995` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `astropy__astropy-6938` | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `astropy__astropy-7746` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| `django__django-10914` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| `django__django-10924` | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ |
| `django__django-11001` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11019` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| `django__django-11039` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11049` | ✓ | ✓ | ✓ | ✓ | ✗ | ✓ |
| `django__django-11099` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-11133` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11179` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11283` | ✗ | ✗ | ✓ | ✓ | ✓ | ✗ |
| `django__django-11422` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11564` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| `django__django-11583` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-11620` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11630` | ✗ | ✗ | ✓ | ✗ | ✓ | ✗ |
| `django__django-11742` | ✗ | ✗ | ✗ | ✓ | ✗ | — |
| `django__django-11797` | ✓ | ✓ | ✗ | ✓ | ✗ | ✓ |
| `django__django-11815` | ✓ | ✓ | ✓ | ✓ | ✗ | ✓ |
| `django__django-11848` | ✗ | ✓ | ✗ | ✓ | ✗ | ✗ |
| `django__django-11905` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| `django__django-11910` | ✗ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `django__django-11964` | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ |
| `django__django-11999` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-12113` | ✓ | ✗ | ✓ | ✓ | ✗ | — |
| `django__django-12125` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-12184` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-12284` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-12286` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-14608` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-15213` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `django__django-15695` | ✗ | ✗ | ✗ | ✗ | ✗ | — |
| `django__django-15781` | ✗ | ✗ | ✗ | ✗ | ✗ | — |
| `django__django-16046` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `matplotlib__matplotlib-23562` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `matplotlib__matplotlib-26011` | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| `pytest-dev__pytest-5221` | ✗ | ✗ | ✗ | ✗ | ✗ | — |
| `scikit-learn__scikit-learn-12471` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `scikit-learn__scikit-learn-25570` | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| `sphinx-doc__sphinx-7686` | ✗ | ✗ | ✗ | ✗ | ✗ | — |
| `sympy__sympy-13177` | ✗ | ✗ | ✗ | ✗ | ✗ | — |
| `sympy__sympy-13647` | ✗ | ✗ | ✗ | ✗ | ✗ | — |
| `sympy__sympy-15346` | ✓ | ✓ | ✗ | ✓ | ✓ | — |
| `sympy__sympy-15678` | ✗ | ✗ | ✓ | ✓ | ✗ | — |
| `sympy__sympy-16106` | ✗ | ✗ | ✗ | ✗ | ✗ | — |

\* spark-ember covered 25/50 only.

## Per-instance cost & tokens (mono / naive / weak-drive)

| Instance | m | n | w | mono$ | naive$ | wd$ | wd weak$ | wd strong$ | mono in→out | naive in→out | wd in / cache / out |
|---|:-:|:-:|:-:|---:|---:|---:|---:|---:|---|---|---|
| `astropy__astropy-12907` | ✓ | ✓ | ✓ | $0.024 | $0.016 | $0.052 | $0.006 | $0.046 | 9k→5k | 15k→4k | 23k / 514k / 9k |
| `astropy__astropy-14182` | ✗ | ✗ | ✗ | $0.046 | $0.069 | $0.082 | $0.007 | $0.075 | 27k→8k | 36k→14k | 45k / 767k / 15k |
| `astropy__astropy-14365` | ✗ | ✗ | ✗ | $0.021 | $0.016 | $0.025 | $0.007 | $0.018 | 13k→3k | 16k→3k | 21k / 204k / 4k |
| `astropy__astropy-14995` | ✓ | ✓ | ✓ | $0.015 | $0.034 | $0.042 | $0.008 | $0.034 | 11k→3k | 27k→5k | 25k / 329k / 11k |
| `astropy__astropy-6938` | ✗ | ✓ | ✓ | $0.010 | $0.044 | $0.026 | $0.006 | $0.020 | 4k→2k | 11k→9k | 13k / 239k / 5k |
| `astropy__astropy-7746` | ✗ | ✗ | ✗ | $0.035 | $0.059 | $0.031 | $0.015 | $0.016 | 15k→8k | 18k→14k | 16k / 228k / 9k |
| `django__django-10914` | ✗ | ✗ | ✗ | $0.108 | $0.070 | $0.067 | $0.019 | $0.048 | 23k→15k | 16k→9k | 25k / 662k / 13k |
| `django__django-10924` | ✓ | ✓ | ✓ | $0.062 | $0.059 | $0.075 | $0.038 | $0.036 | 19k→8k | 13k→9k | 28k / 745k / 14k |
| `django__django-11001` | ✓ | ✓ | ✓ | $0.016 | $0.030 | $0.040 | $0.022 | $0.019 | 5k→3k | 14k→5k | 40k / 321k / 6k |
| `django__django-11019` | ✗ | ✗ | ✗ | $0.531 | $0.129 | $0.449 | $0.236 | $0.213 | 53k→92k | 21k→32k | 121k / 4321k / 104k |
| `django__django-11039` | ✓ | ✓ | ✓ | $0.027 | $0.031 | $0.057 | $0.011 | $0.045 | 11k→3k | 10k→4k | 21k / 545k / 12k |
| `django__django-11049` | ✓ | ✓ | ✓ | $0.063 | $0.077 | $0.054 | $0.016 | $0.039 | 16k→10k | 22k→8k | 26k / 524k / 10k |
| `django__django-11099` | ✓ | ✓ | ✓ | $0.019 | $0.010 | $0.032 | $0.015 | $0.017 | 8k→3k | 4k→2k | 13k / 322k / 5k |
| `django__django-11133` | ✓ | ✓ | ✓ | $0.029 | $0.033 | $0.030 | $0.010 | $0.021 | 11k→4k | 13k→5k | 26k / 262k / 4k |
| `django__django-11179` | ✓ | ✓ | ✓ | $0.028 | $0.012 | $0.031 | $0.021 | $0.010 | 9k→4k | 5k→2k | 12k / 322k / 5k |
| `django__django-11283` | ✗ | ✗ | ✓ | $0.077 | $0.107 | $0.077 | $0.032 | $0.045 | 23k→11k | 27k→22k | 37k / 689k / 17k |
| `django__django-11422` | ✓ | ✓ | ✓ | $0.045 | $0.038 | $0.057 | $0.016 | $0.042 | 20k→6k | 18k→4k | 41k / 529k / 8k |
| `django__django-11564` | ✗ | ✗ | ✗ | $0.111 | $0.098 | $0.152 | $0.035 | $0.117 | 36k→12k | 32k→10k | 62k / 1557k / 24k |
| `django__django-11583` | ✓ | ✓ | ✓ | $0.022 | $0.020 | $0.024 | $0.010 | $0.014 | 8k→2k | 9k→3k | 13k / 237k / 4k |
| `django__django-11620` | ✓ | ✓ | ✓ | $0.234 | $0.069 | $0.112 | $0.018 | $0.094 | 31k→31k | 22k→11k | 33k / 1190k / 18k |
| `django__django-11630` | ✗ | ✗ | ✗ | $0.074 | $0.087 | $0.108 | $0.030 | $0.078 | 24k→12k | 24k→15k | 38k / 1114k / 18k |
| `django__django-11742` | ✗ | ✗ | ✓ | $0.113 | $0.081 | $0.088 | $0.025 | $0.063 | 36k→12k | 34k→9k | 31k / 860k / 17k |
| `django__django-11797` | ✓ | ✓ | ✓ | $0.215 | $0.219 | $0.584 | $0.313 | $0.271 | 30k→31k | 30k→25k | 166k / 6456k / 79k |
| `django__django-11815` | ✓ | ✓ | ✓ | $0.053 | $0.041 | $0.060 | $0.022 | $0.038 | 13k→10k | 11k→7k | 21k / 527k / 15k |
| `django__django-11848` | ✗ | ✓ | ✓ | $0.028 | $0.018 | $0.051 | $0.014 | $0.037 | 6k→7k | 7k→5k | 16k / 494k / 11k |
| `django__django-11905` | ✗ | ✗ | ✗ | $0.064 | $0.080 | $0.063 | $0.018 | $0.045 | 20k→7k | 27k→9k | 22k / 632k / 12k |
| `django__django-11910` | ✗ | ✓ | ✓ | $0.107 | $0.125 | $0.185 | $0.114 | $0.071 | 25k→20k | 24k→27k | 56k / 1767k / 42k |
| `django__django-11964` | ✓ | ✓ | ✗ | $0.079 | $0.073 | $0.127 | $0.019 | $0.108 | 17k→11k | 18k→13k | 45k / 1266k / 24k |
| `django__django-11999` | ✓ | ✓ | ✓ | $0.091 | $0.018 | $0.080 | $0.014 | $0.066 | 20k→12k | 5k→4k | 27k / 858k / 12k |
| `django__django-12113` | ✓ | ✗ | ✓ | $0.176 | $0.267 | $0.268 | $0.115 | $0.153 | 47k→19k | 43k→30k | 93k / 2795k / 43k |
| `django__django-12125` | ✓ | ✓ | ✓ | $0.061 | $0.025 | $0.193 | $0.070 | $0.124 | 14k→8k | 9k→5k | 58k / 2053k / 31k |
| `django__django-12184` | ✓ | ✓ | ✓ | $0.055 | $0.040 | $0.100 | $0.010 | $0.089 | 14k→9k | 13k→7k | 29k / 1037k / 17k |
| `django__django-12284` | ✓ | ✓ | ✓ | $0.029 | $0.048 | $0.092 | $0.027 | $0.065 | 9k→5k | 18k→9k | 28k / 976k / 15k |
| `django__django-12286` | ✓ | ✓ | ✓ | $0.024 | $0.021 | $0.042 | $0.024 | $0.018 | 8k→4k | 8k→4k | 16k / 368k / 11k |
| `django__django-14608` | ✓ | ✓ | ✓ | $0.122 | $0.029 | $0.089 | $0.049 | $0.040 | 29k→12k | 12k→4k | 38k / 948k / 11k |
| `django__django-15213` | ✓ | ✓ | ✓ | $0.180 | $0.272 | $0.193 | $0.052 | $0.141 | 29k→17k | 36k→32k | 52k / 2032k / 34k |
| `django__django-15695` | ✗ | ✗ | ✗ | $0.076 | $0.113 | $0.146 | $0.035 | $0.111 | 13k→13k | 29k→17k | 63k / 1409k / 28k |
| `django__django-15781` | ✗ | ✗ | ✗ | $0.035 | $0.060 | $0.062 | $0.021 | $0.041 | 14k→5k | 14k→9k | 22k / 669k / 8k |
| `django__django-16046` | ✓ | ✓ | ✓ | $0.069 | $0.071 | $0.068 | $0.022 | $0.045 | 14k→11k | 25k→8k | 26k / 698k / 11k |
| `matplotlib__matplotlib-23562` | ✓ | ✓ | ✓ | $0.045 | $0.074 | $0.033 | $0.006 | $0.026 | 13k→9k | 18k→11k | 14k / 291k / 8k |
| `matplotlib__matplotlib-26011` | ✓ | ✓ | ✓ | $0.036 | $0.022 | $0.044 | $0.008 | $0.036 | 8k→7k | 11k→4k | 17k / 422k / 9k |
| `pytest-dev__pytest-5221` | ✗ | ✗ | ✗ | $0.054 | $0.089 | $0.126 | $0.059 | $0.067 | 14k→8k | 23k→11k | 33k / 1300k / 23k |
| `scikit-learn__scikit-learn-12471` | ✓ | ✓ | ✓ | $0.066 | $0.025 | $0.056 | $0.017 | $0.039 | 22k→12k | 12k→4k | 26k / 530k / 11k |
| `scikit-learn__scikit-learn-25570` | ✓ | ✓ | ✓ | $0.165 | $0.074 | $0.085 | $0.021 | $0.064 | 39k→30k | 17k→16k | 56k / 674k / 21k |
| `sphinx-doc__sphinx-7686` | ✗ | ✗ | ✗ | $0.022 | $0.017 | $0.148 | $0.074 | $0.074 | 9k→5k | 8k→2k | 47k / 1410k / 33k |
| `sympy__sympy-13177` | ✗ | ✗ | ✗ | $0.028 | $0.022 | $0.057 | $0.008 | $0.049 | 10k→4k | 10k→4k | 19k / 538k / 13k |
| `sympy__sympy-13647` | ✗ | ✗ | ✗ | $0.032 | $0.015 | $0.034 | $0.008 | $0.025 | 7k→6k | 5k→4k | 21k / 262k / 9k |
| `sympy__sympy-15346` | ✓ | ✓ | ✓ | $0.171 | $0.156 | $0.693 | $0.283 | $0.410 | 24k→21k | 24k→22k | 168k / 7500k / 112k |
| `sympy__sympy-15678` | ✗ | ✗ | ✓ | $0.067 | $0.021 | $0.099 | $0.032 | $0.067 | 14k→11k | 5k→5k | 33k / 965k / 20k |
| `sympy__sympy-16106` | ✗ | ✗ | ✗ | $0.075 | $0.057 | $0.262 | $0.137 | $0.125 | 17k→10k | 18k→7k | 66k / 2957k / 34k |

## Conclusions

### 1. Best strict solve rate on this ladder

- **weak-drive 34/50 (68%)** beats shadow-verify 32, naive 31, mono 29, pi 27.
- Vs mono: recovers **6** mono-misses, loses **1** → net **+5**.
- Vs naive: recovers **4**, loses **1** → net **+3**.

### 2. Higher score costs more — and the bill is mostly strong

- Total $5.75 vs naive $3.28 / mono $3.94 (**those arms are flash-only**; weak-drive adds `deepseek-v4-pro`).
- Of weak-drive spend: **`deepseek-v4-flash` $2.19 (38%)**, **`deepseek-v4-pro` $3.55 (62%)**.
- Pro plan+review drive most **reasoning** tokens; flash does edits/tests cheaper.
- ~97% cache hit rate means repeated context is discounted; still more phases → more tokens.
- Fair cost comparison is **weak-drive total $ vs flash-only arms**, or compare only flash-side tokens if isolating executor efficiency.

### 3. What works

- Strong **plan before edit** reduces wrong-file thrash on many django tasks (rest25 18/25).
- Review/fixup recovers incomplete exec without restarting from zero.
- Smoke 5/5 shows the pipeline is reliable on the easy head.

### 4. What still fails

- Shared hard core (several sympy/astropy/django) unsolved by all pirs arms.
- Tail thrash still expensive (`django-11019` unsolved @ ~$0.45).
- **Bench does not yet run product thrash→advisor mid-loop**, so reactive hybrid is under-measured.

### 5. spark-ember

- On the 25-instance slice, weak-drive **16/25** vs spark-ember **13/25**.
- Parallel explore fan-out did not beat scheduled plan/review here.

### 6. Recommendations

1. Prefer **`weak-drive` with `--model deepseek-v4-flash --plan-model deepseek-v4-pro`** when solve rate is the objective on strict SWE.
2. Keep **naive/mono (`deepseek-v4-flash` only)** as cheap regression baselines.
3. Port **thrash→advisor** into `pirs-bench` to cut tail thrash cost.
4. Cap review REVISE / turns on known thrashy ids.
5. Always report **exact model IDs** and **weak$ vs strong$**, not only total $; dual-model cost is not comparable to flash-only without labeling.

### 7. Non-claims

- Not multi-vendor / open-weight vs frontier (all arms above are DeepSeek; only weak-drive uses **two** model IDs).
- Fair/RAW protocols excluded (different exam, inflated scores).
- spark-ember not run on full 50 (and was flash-only on the 25 it ran).
- Product hybrid mid-loop not attributed in these SWE costs.
