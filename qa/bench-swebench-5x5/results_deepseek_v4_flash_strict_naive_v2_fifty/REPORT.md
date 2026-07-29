# SWE-bench Lite — deepseek-v4-flash STRICT naive v2

**Score: 31/50 (62%)**

## Protocol

Same as strict-v2 (agent-only / issue-only / post-hoc `--check-patch`) but
**`--no-strategy`** (naive undivided agent loop, generic system prompt).

Export-fix binary (no corrupt patches; scrub test edits).

## Comparison

| Run | Score |
|-----|------:|
| Old strict v1 | 20/50 |
| Strict-v2 monolithic | 29/50 |
| **This run (strict-naive-v2)** | **31/50** |
| pi strict | 27/50 |

Head-to-head vs mono on same 50: both agree on 46/50; naive-only +3, mono-only +1
(net +2 — mostly seed variance + a few over-edit/early-stop cases under mono).
