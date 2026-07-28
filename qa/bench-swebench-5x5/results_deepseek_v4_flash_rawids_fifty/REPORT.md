# SWE-bench Lite — deepseek-v4-flash RAW test ids

**Score: 46/50 (92%)**

## Modes explained (read this)

| Mode | Agent sees FAIL_TO_PASS names? | Id hygiene on harness |
|------|-------------------------------|------------------------|
| **Filtered** (42/50) | Yes | `looks_like_test_id` + test_patch recovery |
| **RAW IDs** (this run, **46/50**) | Yes | **None** — dataset strings as-is (`PIRS_RAW_TEST_IDS=1`) |
| **Fair** (`PIRS_FAIR=1` / `--hide-targets`) | **No** | Filtered for grading only |

**RAW is not gold-patch cheating.** It only disables the harness filter that drops non-test-looking strings and recovers ids from `test_patch`. Both filtered and RAW still **spoon-feed** target test ids into the agent prompt; both apply only `test_patch`, never gold `patch`.

See [README.md](README.md) for full comparison and token totals.

## Ablation flag

`PIRS_RAW_TEST_IDS=1` — no `looks_like_test_id`, no test_patch name recovery.

| Instance | Solved | Time (s) | Exit | raw_test_ids |
|---|---|---|---|---|
| `astropy__astropy-12907` | True | 317.4 | 0 | True |
| `astropy__astropy-14182` | True | 400.6 | 0 | True |
| `astropy__astropy-14365` | True | 328.6 | 0 | True |
| `astropy__astropy-14995` | True | 323.0 | 0 | True |
| `astropy__astropy-6938` | True | 91.0 | 0 | True |
| `astropy__astropy-7746` | True | 229.6 | 0 | True |
| `django__django-10914` | True | 198.5 | 0 | True |
| `django__django-10924` | True | 56.3 | 0 | True |
| `django__django-11001` | True | 73.5 | 0 | True |
| `django__django-11019` | True | 351.7 | 0 | True |
| `django__django-11039` | True | 42.4 | 0 | True |
| `django__django-11049` | True | 47.6 | 0 | True |
| `django__django-11099` | True | 34.0 | 0 | True |
| `django__django-11133` | True | 31.4 | 0 | True |
| `django__django-11179` | True | 32.2 | 0 | True |
| `django__django-11283` | False | 22.5 | 1 | True |
| `django__django-11422` | True | 111.8 | 0 | True |
| `django__django-11564` | True | 181.2 | 0 | True |
| `django__django-11583` | True | 39.1 | 0 | True |
| `django__django-11620` | True | 135.5 | 0 | True |
| `django__django-11630` | True | 51.5 | 0 | True |
| `django__django-11742` | True | 101.5 | 0 | True |
| `django__django-11797` | False | 96.2 | 1 | True |
| `django__django-11815` | True | 56.2 | 0 | True |
| `django__django-11848` | True | 44.5 | 0 | True |
| `django__django-11905` | True | 108.0 | 0 | True |
| `django__django-11910` | True | 180.5 | 0 | True |
| `django__django-11964` | True | 148.6 | 0 | True |
| `django__django-11999` | True | 39.7 | 0 | True |
| `django__django-12113` | True | 143.4 | 0 | True |
| `django__django-12125` | True | 48.5 | 0 | True |
| `django__django-12184` | True | 57.5 | 0 | True |
| `django__django-12284` | True | 57.0 | 0 | True |
| `django__django-12286` | True | 55.3 | 0 | True |
| `django__django-14608` | True | 113.3 | 0 | True |
| `django__django-15213` | True | 119.6 | 0 | True |
| `django__django-15695` | False | 91.1 | 1 | True |
| `django__django-15781` | True | 412.3 | 0 | True |
| `django__django-16046` | True | 39.5 | 0 | True |
| `matplotlib__matplotlib-23562` | True | 451.4 | 0 | True |
| `matplotlib__matplotlib-26011` | True | 841.8 | 0 | True |
| `pytest-dev__pytest-5221` | True | 354.9 | 0 | True |
| `scikit-learn__scikit-learn-12471` | True | 99.2 | 0 | True |
| `scikit-learn__scikit-learn-25570` | True | 877.7 | 0 | True |
| `sphinx-doc__sphinx-7686` | True | 336.7 | 0 | True |
| `sympy__sympy-13177` | True | 147.0 | 0 | True |
| `sympy__sympy-13647` | True | 340.9 | 0 | True |
| `sympy__sympy-15346` | True | 223.3 | 0 | True |
| `sympy__sympy-15678` | True | 123.7 | 0 | True |
| `sympy__sympy-16106` | False | 120.0 | 1 | True |
