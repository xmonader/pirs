# SWE-bench Lite fifty — full re-run report (2026-07-22)

**Benchmark:** same 50-instance SWE-bench Lite subset as hero_shrimp extbench.
**Grader:** official `swebench` / extbench `oracle.py` live mode.
**Model:** qwen3.5-plus (DashScope openai-compat).
**Agent:** pirs-bench `solve` in official `sweb.eval` images.

| Field | Run 1 (historical) | Run 2 (full re-run) |
|-------|--------------------|---------------------|
| Scratch | `/tmp/grok-goal-5239d426849a/implementer/swebench/fifty_pirs` | `/tmp/grok-goal-5239d426849a/implementer/swebench/fifty_pirs_full_rerun` |
| Headline | **41/50** (82%) | **40/50** (80% of finished) |
| Character | Mixed binary mid-run + FORCE NO_PATCH re-runs → **41/50** after recoveries | Clean start on fixed binary (django+sympy detectors, soft repro, sanitize, keep-green cap) + sympy FORCE lane |
| hero_shrimp baseline | 32/50 (64%) single-attempt | same reference |

## Headline

| Agent | Resolved | % | Notes |
|-------|----------|---|--------|
| **pirs Run 2 (this report)** | **40/50** | **80%** | n=50 finished; target 50 |
| pirs Run 1 final on-disk | **41/50** | **82%** | After harness fixes + NO_PATCH re-runs |
| hero_shrimp 2026-07-21 | **32/50** | **64%** | Honest single-attempt |

### How to quote

- **Run 2** is the cleaner experiment: fixed harness from the start (commit lineage through `47c7fe9` sympy bare-id fix).
- **Run 1 41/50** included mid-campaign detector landings + FORCE re-runs of early ReproFailed django — not pure single-shot.
- Compare like-for-like on shared instances below.

## By repository (Run 2)

| Repo | Resolved | n | % |
|------|----------|---|---|
| matplotlib | **2** | 2 | 100% |
| pytest-dev | **1** | 1 | 100% |
| sphinx-doc | **1** | 1 | 100% |
| astropy | **5** | 6 | 83% |
| django | **27** | 33 | 82% |
| sympy | **3** | 5 | 60% |
| scikit-learn | **1** | 2 | 50% |
| **Total** | **40** | **50** | **80%** |

## Run 1 → Run 2 transitions (shared instances)

| Transition | Count |
|------------|-------|
| Still RESOLVED | 37 |
| **Gained** (NO→OK) | 3 |
| **Lost** (OK→NO) | 4 |
| Still miss | 6 |

**Gained:** `astropy__astropy-6938`, `django__django-11422`, `django__django-12113`

**Lost:** `django__django-15213`, `django__django-15781`, `scikit-learn__scikit-learn-25570`, `sympy__sympy-15678`

**Still miss:** `astropy__astropy-7746`, `django__django-10914`, `django__django-11019`, `django__django-11797`, `django__django-11910`, `sympy__sympy-13647`

## Run 2 misses taxonomy

| Instance | Kind | Wall (s) | Agent (s) |
|----------|------|----------|-----------|
| `astropy__astropy-7746` | timeout | 1802.9 | None |
| `django__django-10914` | early_fail(<30s) | 5.1 | 2.4 |
| `django__django-11019` | timeout | 1803.4 | None |
| `django__django-11797` | NO_PATCH | 254.7 | 251.3 |
| `django__django-11910` | timeout | 1803.5 | None |
| `django__django-15213` | NO_PATCH | 363.3 | 356.9 |
| `django__django-15781` | early_fail(<30s) | 10.2 | 4.3 |
| `scikit-learn__scikit-learn-25570` | timeout | 1802.5 | None |
| `sympy__sympy-13647` | early_fail(<30s) | 17.5 | 15.2 |
| `sympy__sympy-15678` | timeout | 1803.2 | None |

- Early fails (<30s, often ReproFailed/turns=0): **3**
- Timeouts (~1800s): **5**

## Sympy FORCE re-run (Run 2 lane)

Original campaign sympy often died ~6s at ReproFailed (bare test ids / bin/test).
FORCE lane used `47c7fe9` function-invoke wrapper.

| Instance | R1 | R2 FORCE | Wall R2 | Notes |
|----------|----|----------|---------|-------|
| `sympy__sympy-13177` | RESOLVED/True | RESOLVED/True | 954.3 | agent_s=917.5 solved_int=True |
| `sympy__sympy-13647` | NO_PATCH/False | NO_PATCH/False | 17.5 | agent_s=15.2 solved_int=False |
| `sympy__sympy-15346` | RESOLVED/True | RESOLVED/True | 486.0 | agent_s=394.4 solved_int=True |
| `sympy__sympy-15678` | RESOLVED/True | NO_PATCH/False | 1803.2 | agent_s=None solved_int=False |
| `sympy__sympy-16106` | RESOLVED/True | RESOLVED/True | 702.5 | agent_s=656.4 solved_int=True |

## Insights (Run 1 vs Run 2)

### What held

- **~80% is real** on a clean fixed-binary pass (40/50), not only a recovery narrative. Still **+8 over hero_shrimp 32/50**.
- **Django 27/33 (82%)** matches Run 1 once `runtests.py` detector + soft reproduce are in from the start.
- **Patch sanitize worked:** `django-12113` was oracle **ERROR** (`.pirs` noise) in Run 1 → **RESOLVED** in Run 2.
- **Sympy bare-id wrapper** unblocked real solves (13177, 15346, 16106) that were ~6s ReproFailed before FORCE.

### What regressed / still hurts

| Bucket | Count | Examples | Read |
|--------|-------|----------|------|
| **Timeout (1800s)** | 5 | sklearn-25570, astropy-7746, django-11019/11910, sympy-15678 | Suite too wide / thrash; need early stop |
| **Early fail (<30s)** | 3 | django-10914/15781, sympy-13647 | Harness/repro — agent never starts |
| **Agent ran, no patch** | 2 | django-11797, django-15213 | Model/strategy |

- **Lost vs Run 1 (4):** 25570, 15678, 15781, 15213.
- **Gained vs Run 1 (3):** 6938, 11422, **12113**.
- **Sympy-13647** = detected-wrong (green baseline) → multi-candidate fallthrough + id/test-patch repair.
- **Timeouts dominate remaining headroom** more than pure coding ability.

### Product (not just bench)

1. Detectors must drive `run_tests` / auto-verify, not only oracle scoring.
2. Recovery ladder on detect/repro fail (next runner → CI → soft → agent-discovery flagged).
3. Cmd-fail nudge so wrong shell commands do not loop.

## Improvement backlog (from this comparison)

| Priority | Item | Why |
|----------|------|-----|
| P0 | Sympy wrapper: ensure FAIL_TO_PASS red after test.patch; multi-file resolve; fallback `bin/test` | 13647-class ReproFailed |
| P0 | On ReproFailed try next RunnerSpec before exit | Detected-wrong recovery |
| P1 | Shared discovery for `run_tests` / project tool | Product = bench knowledge |
| P1 | Timeout policy: escalate filter / cut keep-green earlier | sklearn/astropy walls |
| P1 | Django early NO_PATCH autopsy (10914, 15781) | 33 django instances |
| P2 | cmd-fail nudge (bash + weak-model) | Wrong command thrash |
| P2 | Agent-discovery fallback flagged in campaign | Don't waste instances |

## Full Run 2 scorecard

| Instance | Verdict | Wall | Agent solved |
|----------|---------|------|--------------|
| `astropy__astropy-12907` | RESOLVED | 467.8 | True |
| `astropy__astropy-14182` | RESOLVED | 720.4 | True |
| `astropy__astropy-14365` | RESOLVED | 429.4 | True |
| `astropy__astropy-14995` | RESOLVED | 362.2 | True |
| `astropy__astropy-6938` | RESOLVED | 460.1 | True |
| `astropy__astropy-7746` | NO_PATCH | 1802.9 | False |
| `django__django-10914` | NO_PATCH | 5.1 | False |
| `django__django-10924` | RESOLVED | 132.3 | True |
| `django__django-11001` | RESOLVED | 220.5 | True |
| `django__django-11019` | NO_PATCH | 1803.4 | False |
| `django__django-11039` | RESOLVED | 1492.4 | True |
| `django__django-11049` | RESOLVED | 557.7 | True |
| `django__django-11099` | RESOLVED | 256.2 | True |
| `django__django-11133` | RESOLVED | 292.1 | True |
| `django__django-11179` | RESOLVED | 144.5 | True |
| `django__django-11283` | RESOLVED | 218.9 | True |
| `django__django-11422` | RESOLVED | 317.4 | True |
| `django__django-11564` | RESOLVED | 397.3 | True |
| `django__django-11583` | RESOLVED | 162.8 | True |
| `django__django-11620` | RESOLVED | 259.2 | True |
| `django__django-11630` | RESOLVED | 139.2 | True |
| `django__django-11742` | RESOLVED | 411.8 | True |
| `django__django-11797` | NO_PATCH | 254.7 | False |
| `django__django-11815` | RESOLVED | 210.2 | True |
| `django__django-11848` | RESOLVED | 274.3 | True |
| `django__django-11905` | RESOLVED | 389.6 | True |
| `django__django-11910` | NO_PATCH | 1803.5 | False |
| `django__django-11964` | RESOLVED | 174.6 | True |
| `django__django-11999` | RESOLVED | 164.1 | True |
| `django__django-12113` | RESOLVED | 232.3 | True |
| `django__django-12125` | RESOLVED | 223.3 | True |
| `django__django-12184` | RESOLVED | 223.2 | True |
| `django__django-12284` | RESOLVED | 175.1 | True |
| `django__django-12286` | RESOLVED | 132.5 | True |
| `django__django-14608` | RESOLVED | 299.7 | True |
| `django__django-15213` | NO_PATCH | 363.3 | False |
| `django__django-15695` | RESOLVED | 614.4 | True |
| `django__django-15781` | NO_PATCH | 10.2 | False |
| `django__django-16046` | RESOLVED | 110.4 | True |
| `matplotlib__matplotlib-23562` | RESOLVED | 699.0 | True |
| `matplotlib__matplotlib-26011` | RESOLVED | 1014.3 | True |
| `pytest-dev__pytest-5221` | RESOLVED | 413.5 | True |
| `scikit-learn__scikit-learn-12471` | RESOLVED | 320.0 | True |
| `scikit-learn__scikit-learn-25570` | NO_PATCH | 1802.5 | False |
| `sphinx-doc__sphinx-7686` | RESOLVED | 346.7 | True |
| `sympy__sympy-13177` | RESOLVED | 954.3 | True |
| `sympy__sympy-13647` | NO_PATCH | 17.5 | False |
| `sympy__sympy-15346` | RESOLVED | 486.0 | True |
| `sympy__sympy-15678` | NO_PATCH | 1803.2 | False |
| `sympy__sympy-16106` | RESOLVED | 702.5 | True |

---
*Generated 2026-07-22T11:55:03*
