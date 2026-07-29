# SWE-bench Lite — deepseek-v4-flash STRICT-VERIFY v2 (shadow loop)

**In progress** at land time — see `SCORE.txt` / `summary.json` for latest.

## Protocol

- Agent on base (no `test_patch` in workspace), F2P names hidden  
- After each attempt: grade on same package root via reset → apply hidden
  `test_patch` → `check_model_patch` → restore model patch  
- Opaque multi-attempt verdicts (no test ids)  
- Export-fix binary + same-repo grade (not `/tmp` worktree; that path
  falsely ReproFailed on Django editable installs)

## Note

Not 1:1 with agent-only strict — agent gets mid-loop signal. Compare to
strict-v2/naive as “does a fair closed loop help?” not as the same exam.
