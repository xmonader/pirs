# PROJECT-STATE

## Current State
- Done: pi core port (provider, loop, tools, rhai ext, CLI) + rpc mode + orchestrator + weak-model hardening wave (compaction, retry, diet, delegate, phrasing, exec, pack) + multi-model: delegate model override, orchestrator spawn --env. Live proof: strong planner -> delegate(model=weak) -> weak executor (no history leak) -> strong synthesis. usage in events (turn_end/agent_end), register_command + REPL dispatch, fs_append/fs_read/now_millis, five packs (guardrails, audit-log, conductor, context-janitor, reviewer). run_subagent(task, model?) host fn (dedicated-thread runtime), str_join, 21 packs incl. instincts/red-team/arena/relay/hive. 115+6 tests green. .claude/.agents/.pirs skills + commands discovery. Sub-agents inherit policy hooks. MCP client stdio + network: streamable HTTP + legacy SSE transports (mcp-session-id, headers, ${VAR} interpolation) proven live both. tool_dispatch fallback for dynamic rhai tools; subagents/checkpoint/approval/web-tools packs (5 tests). 138 tests. prompt_cache_key for api.openai.com + cache hit-rate in usage. red-team live-verified (caught planted div-zero). 128 tests. Live-proven vs DashScope (qwen3.7-plus planner + glm-4.7 executors, multi-repo trials, UA gate fix). Repo: github.com/xmonader/pirs.
- Next: ratatui TUI; Anthropic provider; model-aware tokenizer for compaction; skills; MCP. Rhai gotchas pinned in tests: backtick ${} interpolation only, trim() mutates in place, call_fn needs statements cleared.
- Blocked: nothing.

## Key Decisions
- Scope: core agent only, no TUI (user choice 2026-07-17). Providers: OpenAI-compatible only. Rhai: tools + hooks.
- Rhai extension convention: `register_tool(name, desc, schema_map)` + script fn `tool_<name>(args)`; hooks `on_tool_call(id,name,args)` returns `#{block,reason}`, `on_tool_result(id,name,result)` returns patch map. Loop hooks: `on_context(messages)->messages`, `on_should_stop(info)->bool`, `on_steering()->msg|()`, `on_follow_up()->[msg]|()`, `on_event(type,data)`. Per-extension Engine + `state_get/state_set/state_has/state_del` store (script fns can't capture scope vars). ASTs kept per-extension with statements cleared (call_fn re-runs statements otherwise).
- grep/find/ls use `ignore`/`globset` crates instead of pi's rg/fd binaries — no auto-download needed.
- Fuzzy edit is whole-line-based (pi is char-range based); exact match is byte-based indexOf equivalent.
- Tool arg validation: jsonschema crate + light string->number/bool/object coercion (pi uses TypeBox Value.Convert).
- Messages/history: no compaction yet (manual-only in pi anyway); sessions stored as JSONL at ~/.pirs/sessions/<encoded-cwd>/<ts>_<pid>.jsonl.

## Architecture Notes
- crates: pirs-ai (types, OpenAI SSE client, retry), pirs-agent (loop, hooks, events, queues, validation), pirs-tools, pirs-rhai (ExtensionHost), pirs (CLI bin, --mode repl|rpc), pirs-orchestrator (daemon).
- RPC wire format (pi-compatible): stdin flat `{id?, type, ...fields}` NDJSON; stdout `{type:"response", command, success, data|error}` + raw AgentEvents (`{type:"agent_start"|message_update|...}`, camelCase fields).
- Orchestrator: UDS at $PIRS_ORCHESTRATOR_DIR/orchestrator.sock (default ~/.pirs/orchestrator), NDJSON one-shot + rpc_stream upgrade with serial command queue; child = `pirs --mode rpc` via $PIRS_RPC_BIN or exe-sibling or PATH; recovery flips persisted online/starting to stopped; session-metadata refresh after prompt/new_session/etc; radius intentionally omitted.
- Loop contract preserved from pi: streamFn never throws (errors arrive as stop_reason error/aborted), stopReason length => fail all tool calls unexecuted, terminate requires unanimous results, parallel mode = end-events in completion order but result messages in source order, steering polled before first LLM call.
- Providers stream via tokio mpsc channel -> BoxStream<StreamEvent>; errors surface as Done{stop_reason:Error}.
