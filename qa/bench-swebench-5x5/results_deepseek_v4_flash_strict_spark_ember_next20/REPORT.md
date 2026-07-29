# spark-ember strict next-20

**Score: 9/20**

Strategy: `spark-ember` · model: `deepseek-v4-flash` · protocol: strict

Phase logs + JSONL `--trace` enabled from mid-batch (`django-11019` has full SPARK×2→EMBER proof).
Trace files are on disk next to results but gitignored (can be 100s of MB).

| Instance | spark-ember | mono-v2 | naive-v2 |
|---|---|---|---|
| `astropy__astropy-12907` | True | True | True |
| `astropy__astropy-14182` | False | False | False |
| `astropy__astropy-14365` | False | False | False |
| `astropy__astropy-6938` | True | False | True |
| `astropy__astropy-7746` | False | False | False |
| `django__django-10914` | False | False | False |
| `django__django-10924` | False | True | True |
| `django__django-11019` | False | False | False |
| `django__django-11039` | True | True | True |
| `django__django-11049` | True | True | True |
| `django__django-11179` | True | True | True |
| `django__django-11283` | False | False | False |
| `django__django-11422` | True | True | True |
| `django__django-11564` | False | False | False |
| `django__django-11620` | True | True | True |
| `django__django-11630` | False | False | False |
| `django__django-11797` | True | True | True |
| `django__django-11815` | True | True | True |
| `django__django-11905` | False | False | False |
| `django__django-11964` | False | True | True |

**Smoke:** 4/5
**Combined smoke+next20:** 13/25
