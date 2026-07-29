# SWE-bench Lite — pi + deepseek-v4-flash STRICT

**Score: 27/50 (54%)**

## Protocol

Same harsh exam as pirs agent-only strict:

- Agent (pi CLI) on base commit with issue prompt only  
- **No** `test_patch` for the agent  
- Grade model patch with `pirs-bench --check-patch` after  

Used as the peer baseline for pirs strict-v2 / naive-v2 (29/50 and 31/50).
