# SWE-bench Lite fifty — Run 3 full FORCE re-run (2026-07-22)

**Purpose:** re-score all 50 after recovery ladder + docstring FAIL_TO_PASS fix + new musl binary, to measure gains **and** regressions vs Run 2.

| Field | Value |
|-------|--------|
| Agent | pirs-bench solve · qwen3.5-plus · DashScope |
| Binary | musl multi-candidate recovery (`a417a05d…` lineage) |
| FORCE | all 50 · conc=2 · max 40 turns · 1800s |
| Scratch | `/tmp/grok-goal-5239d426849a/implementer/swebench/fifty_pirs_full_rerun` |
| Run 2 archive | `run2_final_archive_2026-07-22/` |

## Headline

| Run | Resolved | % | Notes |
|-----|----------|---|--------|
| **Run 3 (this report)** | **46/50** | **92%** | Clean full FORCE on recovery binary |
| Run 2 | **40/50** | **80%** | Prior full re-run (archived) |
| Run 1 | **41/50** | **82%** | Historical + mid-run fixes |
| hero_shrimp | **32/50** | **64%** | Reference single-attempt |

**Delta Run 2 → Run 3: +6 resolved** (40 → 46). **Regressions: 0.**

## By repository (Run 3)

| Repo | Resolved | n | % |
|------|----------|---|---|
| astropy | **6** | 6 | 100% |
| matplotlib | **2** | 2 | 100% |
| pytest-dev | **1** | 1 | 100% |
| scikit-learn | **2** | 2 | 100% |
| sphinx-doc | **1** | 1 | 100% |
| sympy | **5** | 5 | 100% |
| django | **29** | 33 | 88% |
| **Total** | **46** | **50** | **92%** |

## Run 2 → Run 3 transitions

| Gained (NO→OK) | 6 |
| Lost (OK→NO) | 0 |
| Still miss | 4 |

**Gained:** `scikit-learn__scikit-learn-25570`, `django__django-10914`, `sympy__sympy-15678`, `django__django-15781`, `astropy__astropy-7746`, `sympy__sympy-13647`

**No regressions** — every Run 2 RESOLVED stayed RESOLVED.

**Still miss:** `django__django-11019`, `django__django-11797`, `django__django-15213`, `django__django-11910`

## Run 3 misses

| Instance | Wall (s) | Kind |
|----------|----------|------|
| `django__django-11019` | 1804.4 | timeout |
| `django__django-11797` | 330.3 | no_patch |
| `django__django-11910` | 1804.8 | timeout |
| `django__django-15213` | 804.1 | no_patch |

## Insights

1. **Recovery binary is a real lift:** +6 absolute over Run 2 (40→46), **+14 over hero_shrimp 32/50**.
2. **No regressions** on the full FORCE pass — safe to treat multi-candidate + id recovery as default.
3. **Recovered hard cases:** sympy-13647 (was ReproFailed), sympy-15678 + sklearn-25570 + astropy-7746 (timeouts), django-15781 (docstring FAIL_TO_PASS), django-10914 (early).
4. **Remaining 4 are sticky django:** two hard timeouts (11019, 11910), two agent-no-patch (15213, 11797) — not detector zero-turns.
5. **Product path still open:** share detectors with `run_tests`; timeout/early-stop policy for long django media suites.

## Full Run 3 scorecard

| Instance | Verdict | Wall |
|----------|---------|------|
| `astropy__astropy-12907` | RESOLVED | 586.0 |
| `astropy__astropy-14182` | RESOLVED | 876.5 |
| `astropy__astropy-14365` | RESOLVED | 423.6 |
| `astropy__astropy-14995` | RESOLVED | 414.7 |
| `astropy__astropy-6938` | RESOLVED | 240.3 |
| `astropy__astropy-7746` | RESOLVED | 1033.2 |
| `django__django-10914` | RESOLVED | 447.3 |
| `django__django-10924` | RESOLVED | 168.0 |
| `django__django-11001` | RESOLVED | 207.5 |
| `django__django-11019` | NO_PATCH | 1804.4 |
| `django__django-11039` | RESOLVED | 114.7 |
| `django__django-11049` | RESOLVED | 110.6 |
| `django__django-11099` | RESOLVED | 65.4 |
| `django__django-11133` | RESOLVED | 141.6 |
| `django__django-11179` | RESOLVED | 111.5 |
| `django__django-11283` | RESOLVED | 206.2 |
| `django__django-11422` | RESOLVED | 240.6 |
| `django__django-11564` | RESOLVED | 407.9 |
| `django__django-11583` | RESOLVED | 103.0 |
| `django__django-11620` | RESOLVED | 156.8 |
| `django__django-11630` | RESOLVED | 166.8 |
| `django__django-11742` | RESOLVED | 175.0 |
| `django__django-11797` | NO_PATCH | 330.3 |
| `django__django-11815` | RESOLVED | 141.2 |
| `django__django-11848` | RESOLVED | 403.1 |
| `django__django-11905` | RESOLVED | 170.2 |
| `django__django-11910` | NO_PATCH | 1804.8 |
| `django__django-11964` | RESOLVED | 109.8 |
| `django__django-11999` | RESOLVED | 136.5 |
| `django__django-12113` | RESOLVED | 240.9 |
| `django__django-12125` | RESOLVED | 186.6 |
| `django__django-12184` | RESOLVED | 130.1 |
| `django__django-12284` | RESOLVED | 151.9 |
| `django__django-12286` | RESOLVED | 116.4 |
| `django__django-14608` | RESOLVED | 180.9 |
| `django__django-15213` | NO_PATCH | 804.1 |
| `django__django-15695` | RESOLVED | 176.5 |
| `django__django-15781` | RESOLVED | 408.2 |
| `django__django-16046` | RESOLVED | 67.1 |
| `matplotlib__matplotlib-23562` | RESOLVED | 790.7 |
| `matplotlib__matplotlib-26011` | RESOLVED | 901.6 |
| `pytest-dev__pytest-5221` | RESOLVED | 221.2 |
| `scikit-learn__scikit-learn-12471` | RESOLVED | 143.4 |
| `scikit-learn__scikit-learn-25570` | RESOLVED | 1145.4 |
| `sphinx-doc__sphinx-7686` | RESOLVED | 1097.3 |
| `sympy__sympy-13177` | RESOLVED | 365.4 |
| `sympy__sympy-13647` | RESOLVED | 687.8 |
| `sympy__sympy-15346` | RESOLVED | 578.4 |
| `sympy__sympy-15678` | RESOLVED | 457.4 |
| `sympy__sympy-16106` | RESOLVED | 265.2 |

*Generated 2026-07-22T15:10:39*
