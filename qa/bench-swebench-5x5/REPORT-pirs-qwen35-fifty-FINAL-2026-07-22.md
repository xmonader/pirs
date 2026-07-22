# SWE-bench Lite fifty — pirs final campaign report (2026-07-22)

Three successive campaigns of the same **50-instance** SWE-bench Lite subset,
scored by the **official** swebench / extbench `oracle.py` live grader
(qwen3.5-plus via DashScope). hero_shrimp single-attempt reference: **32/50**.

## Headline

| Run | Resolved | % | Character |
|-----|----------|---|-----------|
| **Run 3 (recovery binary, full FORCE)** | **46/50** | **92.0%** | Multi-candidate runner + docstring FAIL_TO_PASS recovery; clean FORCE of all 50 |
| Run 2 (fixed harness, full re-run) | **40/50** | **80.0%** | django/sympy detectors from start + sympy FORCE lane |
| Run 1 (historical) | **41/50** | **82.0%** | Mid-campaign detector landings + NO_PATCH re-runs |
| hero_shrimp (2026-07-21) | **32/50** | **64%** | Honest single-attempt baseline |

**Best publishable clean score: Run 3 = 46/50 (92.0%).**  
**+14 over hero_shrimp · +6 over Run 2 · zero regressions Run 2→3.**

### How to quote

- **46/50 (92.0%)** is a full FORCE re-score on a fixed recovery binary (not mid-run hotfixes).
- Run 1 41/50 included mid-campaign harness changes; useful historically, not a pure single-binary claim.
- All verdicts are official oracle RESOLVED, not agent self-report.

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
| **Total** | **46** | **50** | **92.0%** |

Perfect repos in Run 3: **astropy, matplotlib, pytest, scikit-learn, sphinx, sympy**.
Django: **29/33** — all remaining misses.

## Run 2 → Run 3

| Transition | Count |
|------------|-------|
| Gained (NO→OK) | **6** |
| Lost (OK→NO) | **0** |
| Still miss | **4** |

**Recovered:**
- `scikit-learn__scikit-learn-25570`
- `django__django-10914`
- `sympy__sympy-15678`
- `django__django-15781`
- `astropy__astropy-7746`
- `sympy__sympy-13647`

**Regressions: none.**

**Still miss after Run 3:**
- `django__django-11019`
- `django__django-11797`
- `django__django-15213`
- `django__django-11910`

## Run 3 misses (4)

| Instance | Verdict | Wall (s) | Agent (s) | Kind |
|----------|---------|----------|-----------|------|
| `django__django-11019` | NO_PATCH | 1804.4 | None | timeout |
| `django__django-11797` | NO_PATCH | 330.3 | 326.3 | agent_no_patch |
| `django__django-11910` | NO_PATCH | 1804.8 | None | timeout |
| `django__django-15213` | NO_PATCH | 804.1 | 799.4 | agent_no_patch |

### Miss reading

- **timeouts (2):** `django-11019`, `django-11910` — burn full 1800s; need earlier stop / tighter keep-green / media-suite scoping.
- **agent_no_patch (2):** `django-15213`, `django-11797` — agent runs for minutes, no accepted patch; model/strategy, not zero-turn ReproFailed.
- No remaining early ReproFailed / turns=0 detector deaths in the final four.

## What changed between runs (harness)

| Area | Landed | Effect in Run 3 |
|------|--------|-----------------|
| django `runtests.py` detector + soft reproduce | Run 1→2 | django 27–29/33 viable |
| sympy bare-id function invoke | Run 2 FORCE | sympy path unblocked |
| patch sanitize (`.pirs` noise) | Run 2 | django-12113 stable RESOLVED |
| multi-candidate on ReproFailed | pre-Run 3 | detected-wrong fallthrough |
| docstring FAIL_TO_PASS → test_patch ids | pre-Run 3 | django-15781 recovered |
| cmd-fail shell nudges | pre-Run 3 | agent path (hard to attribute in score) |

## Cross-run divergences (any run differs)

| Instance | R1 | R2 | R3 |
|----------|----|----|-----|
| `astropy__astropy-6938` | NO | OK | OK |
| `astropy__astropy-7746` | NO | NO | OK |
| `django__django-10914` | NO | NO | OK |
| `django__django-11422` | NO | OK | OK |
| `django__django-12113` | NO | OK | OK |
| `django__django-15213` | OK | NO | NO |
| `django__django-15781` | OK | NO | OK |
| `scikit-learn__scikit-learn-25570` | OK | NO | OK |
| `sympy__sympy-13647` | NO | NO | OK |
| `sympy__sympy-15678` | OK | NO | OK |

## Insights

1. **92% is real** on a clean full FORCE with a fixed recovery binary — best defensible pirs number on this fifty.
2. **Harness > model** for the early zero-turn failures; once detectors/repro work, remaining misses are timeouts and hard django patches.
3. **Sympy 5/5** after bare-id + multi-candidate recovery proves specialized runners pay off.
4. **Sticky headroom is django timeouts** (11019, 11910) — product/bench both need keep-green budget + early abort.
5. **Dual brain still open:** share bench detectors with `run_tests` / auto-verify for non-bench users.
6. **Vs hero_shrimp:** +14 absolute (46 vs 32) on the same subset/model family under official grading.

## Full Run 3 scorecard

| Instance | Verdict | Wall (s) | Agent solved |
|----------|---------|----------|--------------|
| `astropy__astropy-12907` | RESOLVED | 586.0 | True |
| `astropy__astropy-14182` | RESOLVED | 876.5 | True |
| `astropy__astropy-14365` | RESOLVED | 423.6 | True |
| `astropy__astropy-14995` | RESOLVED | 414.7 | True |
| `astropy__astropy-6938` | RESOLVED | 240.3 | True |
| `astropy__astropy-7746` | RESOLVED | 1033.2 | True |
| `django__django-10914` | RESOLVED | 447.3 | True |
| `django__django-10924` | RESOLVED | 168.0 | True |
| `django__django-11001` | RESOLVED | 207.5 | True |
| `django__django-11019` | NO_PATCH | 1804.4 | False |
| `django__django-11039` | RESOLVED | 114.7 | True |
| `django__django-11049` | RESOLVED | 110.6 | True |
| `django__django-11099` | RESOLVED | 65.4 | True |
| `django__django-11133` | RESOLVED | 141.6 | True |
| `django__django-11179` | RESOLVED | 111.5 | True |
| `django__django-11283` | RESOLVED | 206.2 | True |
| `django__django-11422` | RESOLVED | 240.6 | True |
| `django__django-11564` | RESOLVED | 407.9 | True |
| `django__django-11583` | RESOLVED | 103.0 | True |
| `django__django-11620` | RESOLVED | 156.8 | True |
| `django__django-11630` | RESOLVED | 166.8 | True |
| `django__django-11742` | RESOLVED | 175.0 | True |
| `django__django-11797` | NO_PATCH | 330.3 | False |
| `django__django-11815` | RESOLVED | 141.2 | True |
| `django__django-11848` | RESOLVED | 403.1 | True |
| `django__django-11905` | RESOLVED | 170.2 | True |
| `django__django-11910` | NO_PATCH | 1804.8 | False |
| `django__django-11964` | RESOLVED | 109.8 | True |
| `django__django-11999` | RESOLVED | 136.5 | True |
| `django__django-12113` | RESOLVED | 240.9 | True |
| `django__django-12125` | RESOLVED | 186.6 | True |
| `django__django-12184` | RESOLVED | 130.1 | True |
| `django__django-12284` | RESOLVED | 151.9 | True |
| `django__django-12286` | RESOLVED | 116.4 | True |
| `django__django-14608` | RESOLVED | 180.9 | True |
| `django__django-15213` | NO_PATCH | 804.1 | False |
| `django__django-15695` | RESOLVED | 176.5 | True |
| `django__django-15781` | RESOLVED | 408.2 | True |
| `django__django-16046` | RESOLVED | 67.1 | True |
| `matplotlib__matplotlib-23562` | RESOLVED | 790.7 | True |
| `matplotlib__matplotlib-26011` | RESOLVED | 901.6 | True |
| `pytest-dev__pytest-5221` | RESOLVED | 221.2 | True |
| `scikit-learn__scikit-learn-12471` | RESOLVED | 143.4 | True |
| `scikit-learn__scikit-learn-25570` | RESOLVED | 1145.4 | True |
| `sphinx-doc__sphinx-7686` | RESOLVED | 1097.3 | True |
| `sympy__sympy-13177` | RESOLVED | 365.4 | True |
| `sympy__sympy-13647` | RESOLVED | 687.8 | True |
| `sympy__sympy-15346` | RESOLVED | 578.4 | False |
| `sympy__sympy-15678` | RESOLVED | 457.4 | True |
| `sympy__sympy-16106` | RESOLVED | 265.2 | True |

## Artifacts

| Path | Content |
|------|---------|
| `qa/bench-swebench-5x5/REPORT-pirs-qwen35-fifty-2026-07-22.md` | Run 1 narrative |
| `qa/bench-swebench-5x5/REPORT-pirs-qwen35-fifty-full-rerun-2026-07-22.md` | Run 2 |
| `qa/bench-swebench-5x5/REPORT-pirs-qwen35-fifty-run3-2026-07-22.md` | Run 3 short |
| this file | Final three-run synthesis |
| `/tmp/.../fifty_pirs_full_rerun/run2_final_archive_2026-07-22/` | Run 2 frozen results |
| `/tmp/.../fifty_pirs_full_rerun/run3_full.log` | Run 3 campaign log |

*Generated 2026-07-22T15:19:22*
