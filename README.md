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

Loop hooks: `on_context(messages)`, `on_should_stop(info)`, `on_steering()`, `on_follow_up()`, `on_event(type, data)` (events carry token usage). State per extension via `state_get`/`state_set`; shell out via `exec(cmd, timeout_secs)`; file append/read via `fs_append`/`fs_read`; register slash commands via `register_command(name, desc)` + `fn cmd_<name>(args)` — dispatched by the REPL.

Shipped packs in `examples/extensions/`:

| Pack | Purpose |
|---|---|
| `weak-model.rhai` | loop detector, verify-after-edit, plan pinning |
| `guardrails.rhai` | block destructive bash patterns, ask-first policy |
| `audit-log.rhai` | tool calls + results to `~/.pirs/audit.jsonl` |
| `conductor.rhai` | strong-planner/weak-executor guidance + plan tool |
| `context-janitor.rhai` | shrink stale giant tool outputs in outgoing context |
| `reviewer.rhai` | force a review pass after file edits before run ends |
| `instincts.rhai` | learn failure→fix pairs, steer away from repeats |
| `session-handoff.rhai` | next-session brief carried via `.pirs/handoff.md` |
| `failure-diary.rhai` | known-pitfalls pin built from tool failures |
| `red-team.rhai` | fresh-context adversary attacks your diff before run ends |
| `shadow-verify.rhai` | re-runs test commands, flags claimed-vs-actual discrepancies |
| `spec-check.rhai` | pins ACCEPTANCE.md, forces item-by-item verification |
| `semantic-bookmarks.rhai` | model-pinned notes at context tail (max 5) |
| `diff-shield.rhai` | merges consecutive same-tool results to save context |
| `chapter-spine.rhai` | weak-model chapter titles keep long sessions coherent |
| `repo-pulse.rhai` | fresh branch/dirty-files pin after every edit |
| `env-doctor.rhai` | blocks calls to missing toolchains with install hints |
| `cost-sentinel.rhai` | token budget: warn at 50%, hard-stop at cap |
| `critic-arena.rhai` | same task on two models, you judge the answers |
| `relay-race.rhai` | draft→critique→finalize pipeline as one tool |
| `hive-note.rhai` | shared blackboard for multi-instance coordination |

Scripts can also spawn fresh-context sub-agents themselves: `run_subagent(task, model?)`.

rhai gotchas (pinned by tests): interpolation only in backtick strings `` `${x}` ``; string methods like `trim()` mutate in place; no `let mut`; arrays have no `join` — use `str_join(arr, sep)` or a loop; array property access clones (write whole entries back); `const` doesn't resolve inside nested closures.

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
