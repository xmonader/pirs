# pirs

A Rust port of the [pi agent harness](https://github.com/earendil-works/pi): an OpenAI-compatible coding agent with a streaming agent loop, built-in coding tools, [rhai](https://rhai.rs)-script extensibility, a headless RPC mode, and a multi-instance orchestrator.

Status: **alpha**. The core is ported and tested (70+ tests); the TUI, Anthropic/Google providers, compaction, and skills are not yet ported.

## Quickstart

```bash
cargo build --release
export OPENAI_API_KEY=...            # or --api-key; OPENAI_BASE_URL for compatible endpoints

./target/release/pirs                          # interactive REPL
./target/release/pirs "fix the failing test"   # one-shot
```

REPL commands: `/model`, `/export`, `/compact`, `/help`, `/quit`; `!cmd` runs a local command and records it in context (`!!cmd` skips recording). Type while the agent is working to steer it. Sessions persist as JSONL under `~/.pirs/sessions/` (`--resume`).

Hardening flags: `--tool-diet` (start with core tools only; the model loads more via `use_tool`), `--sequential` (one tool call at a time), `--no-compaction` / `--context-window N`, `--max-retries N` (also retries empty/garbage completions). A `delegate` tool runs subtasks in fresh-context sub-agents — with an optional `model` override, this gives strong-planner/weak-executor routing in one process (sub-agents see no parent history, return only their answer). The orchestrator's `spawn --env KEY=VAL` (repeatable) configures per-instance providers/models for mixed fleets. and auto-compaction summarizes old history when the context window fills. `examples/extensions/weak-model.rhai` adds loop-detection, verify-after-edit, and plan pinning as a script pack.

## Extensions (rhai)

Drop `.rhai` files in `.pirs/extensions/` or `~/.pirs/extensions/`:

```rhai
register_tool("word_count", "Count words", #{
    type: "object",
    properties: #{ text: #{ type: "string" } },
    required: ["text"]
});

fn tool_word_count(args) {
    `${args.text.split(" ").len()} words`   // note: backtick ${} interpolation
}

fn on_tool_call(id, name, args) {
    if name == "bash" && args.command.contains("rm -rf") {
        return #{ block: true, reason: "rejected by policy" };
    }
    ()
}
```

Loop hooks: `on_context(messages)`, `on_should_stop(info)`, `on_steering()`, `on_follow_up()`, `on_event(type, data)`. State per extension via `state_get`/`state_set`; shell out via `exec(cmd, timeout_secs)`. See `examples/extensions/` (word_count, weak-model pack).

## Orchestrator

Run fleets of headless agents (`pirs --mode rpc`, pi-compatible JSONL RPC):

```bash
pirs-orchestrator serve &
pirs-orchestrator spawn --cwd /repo --label demo
pirs-orchestrator rpc <id> '{"type":"prompt","message":"run the tests"}'
pirs-orchestrator rpc-stream <id>        # raw JSONL bridge
pirs-orchestrator stop <id>
```

## Crates

| Crate | Contents |
|---|---|
| `pirs-ai` | message types, OpenAI-compatible SSE streaming client, tool-call accumulation, retries |
| `pirs-agent` | agent loop, tool execution, hooks, events, steering/follow-up queues |
| `pirs-tools` | `bash`, `read`, `edit`, `write`, `grep`, `find`, `ls` |
| `pirs-rhai` | rhai extension host: script tools, tool policy, loop hooks |
| `pirs` | CLI (`--mode repl\|rpc`) |
| `pirs-orchestrator` | daemon + CLI for spawning/managing headless instances |

## Development

```bash
make build   # cargo build
make test    # cargo test --workspace
make lint    # clippy -D warnings
```

## Notable divergences from pi

- OpenAI-compatible providers only (for now); grep/find are native Rust instead of rg/fd binaries; fuzzy `edit` is line-based; compaction is trigger-based (no model-aware tokenizer); no radius cloud presence; MIT licensed.
