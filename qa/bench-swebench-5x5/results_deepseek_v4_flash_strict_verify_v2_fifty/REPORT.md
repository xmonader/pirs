# SWE-bench Lite — deepseek-v4-flash STRICT-VERIFY v2 (shadow loop)

**Score: 32/50 (64%)**

## Protocol

- Agent on base (no `test_patch` in workspace), F2P names hidden
- After each attempt: grade on same package root via reset → apply hidden
  `test_patch` → `check_model_patch` → restore model patch
- Opaque multi-attempt verdicts (no test ids)
- Export-fix binary + same-repo grade (not `/tmp` worktree)

## Comparison

| Run | Score |
|-----|------:|
| **This run (shadow-verify-v2)** | **32/50** |
| Strict-naive-v2 (one-shot) | 31/50 |
| Strict-v2 mono (one-shot) | 29/50 |
| pi strict | 27/50 |
| Old strict v1 | 20/50 |

Not 1:1 with agent-only strict — agent gets mid-loop opaque signal.
