# weak-drive strict next-20

**Score: 11/20**

| | |
|---|---|
| Strategy | `weak-drive` only |
| **Weak model** | **`deepseek-v4-flash`** (`--model`, exec + fixup) |
| **Strong model** | **`deepseek-v4-pro`** (`--plan-model`, plan + review) |
| Protocol | strict agent-only |

Same 20 instances as spark-ember next20 (smoke 5 excluded).

| Instance | solved | elapsed_s |
|---|---|---|
| `astropy__astropy-12907` | True | 389.8 |
| `astropy__astropy-14182` | False | 476.2 |
| `astropy__astropy-14365` | False | 333.9 |
| `astropy__astropy-6938` | True | 163.8 |
| `astropy__astropy-7746` | False | 319.2 |
| `django__django-10914` | False | 213.6 |
| `django__django-10924` | True | 231.1 |
| `django__django-11019` | False | 1171.2 |
| `django__django-11039` | True | 192.7 |
| `django__django-11049` | True | 158.3 |
| `django__django-11179` | True | 87.5 |
| `django__django-11283` | True | 270.9 |
| `django__django-11422` | True | 145.8 |
| `django__django-11564` | False | 370.0 |
| `django__django-11620` | True | 307.2 |
| `django__django-11630` | False | 367.0 |
| `django__django-11797` | True | 1158.6 |
| `django__django-11815` | True | 252.4 |
| `django__django-11905` | False | 206.7 |
| `django__django-11964` | False | 395.2 |

**Smoke:** 5/5
**Combined smoke+next20:** 16/25
