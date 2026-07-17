## session checkpoint\nGoodbye! Feel free to return if you need help with Rust, coding, or anything else.\n\n## session checkpoint\nThe tool said: done!\n\n## session checkpoint\n## Findings for `error_result`

**Symbol definition:**
- `fn error_result` at `crates/pirs-agent/src/agent_loop.rs:377`

**Callers (4 functions):**
1. `run_agent_loop` (line 52)
2. `prepare_call` (line 423)
3. `finalize_result` (line 486)
4. `execute_tool_calls` (line 533)

All callers are in the same file (`agent_loop.rs`), suggesting `error_result` is a helper function for handling errors across the agent loop's main operations.\n\n