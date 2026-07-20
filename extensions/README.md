# Extension catalog

Rhai extension packs for `pirs`. Each `.rhai` file registers custom tools,
slash commands, and lifecycle hooks against the same host API. They are the
working reference for what the extension surface can do — copy one, edit it, or
drop it into `.pirs/extensions/` (project) or `~/.pirs/extensions/` (global) to
load it.

These are **not** auto-loaded from this directory; it is a catalog. To run one,
place it under a `.pirs/extensions/` directory. The integration tests in
`crates/pirs-rhai/tests/` load them straight from here to prove each still
compiles and behaves.

## Safety & guardrails

| Extension | What it does |
|-----------|--------------|
| `sandbox.rhai` | Overrides `bash` with an OS-level sandbox (bubblewrap/Seatbelt, falling back to Docker/Podman if bwrap can't start): read-only filesystem outside the working dir, no network by default — or a domain allowlist (`.pirs/sandbox-allowlist.txt`) enforced by a local CONNECT proxy on Docker/Podman. |
| `guardrails.rhai` | Block destructive commands; the model must ask the user first. |
| `path-guard.rhai` | Block sensitive commands (`rm`/`chmod`/`chown`/etc, `find -exec`/`-delete`) whose targets are outside the working directory. |
| `approval.rhai` | Sensitive tool calls require explicit user approval. |
| `approval2.rhai` | Semantic blast-radius: a sub-agent judges how risky a command is. |
| `diff-shield.rhai` | Merge consecutive same-tool results to compress context. |
| `dirty-guard.rhai` | Commit pre-existing user WIP before the AI edits a file. |
| `env-doctor.rhai` | Block tool calls for missing toolchains, with install hints. |
| `safe_edit.rhai` | Editor-mode edits: a narrow prompt applies a single focused diff. |

## Verification & review

| Extension | What it does |
|-----------|--------------|
| `review-gate.rhai` | An independent reviewer that can REFUSE completion. |
| `reviewer.rhai` | After file edits, force a review pass before the run ends. |
| `critic.rhai` | Interleaved mid-run critic: every N edits, a background pass. |
| `critic-arena.rhai` | Two models answer the same task; you judge. |
| `red-team.rhai` | After edits, a fresh-context adversary attacks the changes. |
| `shadow-verify.rhai` | Re-run test commands and compare against claimed results. |
| `verify-guard.rhai` | A passing verify command that ran ZERO tests does not count. |
| `verify-impact.rhai` | Graph-scoped verification after a successful edit. |
| `mutation-guard.rhai` | Self-verifying codegen via mutation testing. |
| `spec-check.rhai` | Pin `ACCEPTANCE.md`; force a checklist pass before ending. |
| `relay-race.rhai` | Draft → critique → finalize pipeline as a single tool. |

## Cost & budget

| Extension | What it does |
|-----------|--------------|
| `cost-sentinel.rhai` | Cumulative token budget: warn at 50%, hard-stop at cap. |
| `spend-caps.rhai` | Persistent USD spending caps (daily/monthly), hydrated on start. |

## Multi-agent & orchestration

| Extension | What it does |
|-----------|--------------|
| `conductor.rhai` | Strong-planner / weak-executor guidance pack. |
| `weak-model.rhai` | Loop hardening for weaker models. |
| `subagents.rhai` | Named sub-agents from `.pirs/agents/*.md` (and `~/.pirs/agents`). |
| `fork.rhai` | Cache-warm delegates: the sub-agent inherits the current context. |
| `workflow.rhai` | A rerunnable multi-agent workflow: fan out reviews over a set. |
| `swarm.rhai` | Work-queue discipline over the hive: a queen posts packets. |
| `hive-note.rhai` | Shared blackboard for coordinating multiple pirs instances. |

## Context management

| Extension | What it does |
|-----------|--------------|
| `context-janitor.rhai` | Shrink stale giant tool outputs in the outgoing context. |
| `semantic-bookmarks.rhai` | Model-managed pinned notes at the context tail. |
| `repo-pulse.rhai` | Keep a fresh repo-state pin (branch, dirty files) in context. |
| `failure-diary.rhai` | Log tool failures and pin recent pitfalls in context. |
| `instincts.rhai` | Learn (failure → fix) pairs and steer away from repeats. |
| `btw.rhai` | Side questions that never enter the main history. |

## Persistence, memory & handoff

| Extension | What it does |
|-----------|--------------|
| `goal.rhai` | First-class session goals: set, pin, verify, persist. |
| `checkpoint.rhai` | Periodic session snapshots + `/checkpoints` + `/restore`. |
| `checkpoints.rhai` | VCS-free per-file checkpoints: every edit is restorable. |
| `rollback.rhai` | Snapshot the worktree every turn via `commit-tree` (no touch to index). |
| `dmail.rhai` | Model-initiated time travel (D-Mail). |
| `session-handoff.rhai` | Carry context between sessions via `.pirs/handoff.md`. |
| `skill-crystallizer.rhai` | After a successful run, distill what worked into a skill. |

## Provenance & audit

| Extension | What it does |
|-----------|--------------|
| `blame.rhai` | Line-level provenance: attribute each changed line to its turn. |
| `audit-log.rhai` | Append every tool call and result (full content) to `~/.pirs/audit.jsonl`. |
| `runs.rhai` | Durable run records: each run appends to `~/.pirs/runs/<ts>.jsonl`. |
| `telemetry.rhai` | Metadata-only usage stats (counts, tokens, stop reasons) to `~/.pirs/telemetry.jsonl` — never prompt/tool content. |

## Authoring & misc

| Extension | What it does |
|-----------|--------------|
| `chapter-spine.rhai` | A weak model writes one-line chapter titles into a spine. |
| `web-tools.rhai` | `web_fetch` and `web_search` via curl. |
| `word_count.rhai` | Reference example: custom tool, safety hook, loop-behavior hooks. |
