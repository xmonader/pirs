# PROJECT-STATE

## Current State
- Done: Rust port of pi core (OpenAI-compat provider, agent loop, 7 built-in tools, rhai extensions, CLI REPL) + `pirs --mode rpc` headless instances + `pirs-orchestrator` (daemon/supervisor/UDS/CLI). 70 tests green, clippy -D warnings clean, e2e verified (mock OpenAI + real daemon spawn/prompt/stream/stop).
- Next: ratatui TUI; Anthropic provider; compaction; edit-tool file mutation queue parity; `!` bash-mode editor integration; `/compact` command. Orchestrator gaps: no radius (cloud presence, intentionally omitted), no `extension_ui_request` routing (pirs has no TUI prompts), rpc commands fork/clone/tree/compact/export_html unsupported.
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
