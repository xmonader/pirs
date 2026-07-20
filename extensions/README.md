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
| `guardrails.rhai` | Hard-blocks a fixed list of known-catastrophic patterns (`rm -rf /`, `curl \| bash`, force-push, ...) regardless of location — no ask, just refuses. |
| `path-guard.rhai` | Blocks otherwise-ordinary commands (`rm`/`chmod`/`chown`/etc, `find -exec`/`-delete`) whose *target resolves outside the working directory* — catches what a fixed pattern list can't (structural, not pattern-based). |
| `blast-radius-judge.rhai` | Semantic blast-radius: a sub-agent judges how risky a command is against the actual environment (git status/stash), not a fixed list. |
| `diff-shield.rhai` | Merge consecutive same-tool results to compress context. |
| `dirty-guard.rhai` | Commit pre-existing user WIP before the AI edits a file. |
| `env-doctor.rhai` | Block tool calls for missing toolchains, with install hints. |
| `safe-edit.rhai` | Editor-mode edits: a narrow prompt applies a single focused diff. |

The four safety packs above are deliberately complementary layers (fixed denylist, structural path check, semantic judge, OS-level sandbox), not overlapping alternatives — combine as many as you want. For interactive approval prompting, use the native `--approval ask` flag rather than a pack; a rhai-based reimplementation of that (`approval.rhai`) used to live here and was retired for duplicating it with a clunkier chat-text ticket flow.

## Verification & review

| Extension | What it does |
|-----------|--------------|
| `review-gate.rhai` | An independent, fresh-context sub-agent reviews every diff against the original request AND adversarially (bugs/edge-cases/race-conditions), and can REFUSE completion. |
| `critic.rhai` | Interleaved mid-run critic: every N edits, a background pass. |
| `critic-arena.rhai` | Two models answer the same task; you judge. |
| `shadow-verify.rhai` | Re-run test commands and compare against claimed results. |
| `verify-guard.rhai` | A passing verify command that ran ZERO tests does not count. |
| `verify-impact.rhai` | Graph-scoped verification after a successful edit. |
| `mutation-guard.rhai` | Self-verifying codegen via mutation testing. |
| `spec-check.rhai` | Pin `ACCEPTANCE.md`; force a checklist pass before ending. |
| `relay-race.rhai` | Draft → critique → finalize pipeline as a single tool. |

Two packs were retired in favor of `review-gate.rhai`, which now covers both: `reviewer.rhai` (same-model self-review reminder — weaker, the model that wrote the bug reviews its own work) and `red-team.rhai` (a separate adversarial-attack pass with the same edit/write trigger and the same fresh-sub-agent-reviews-the-diff shape — its prompt is now folded into review-gate.rhai's own, rather than paying for two overlapping review calls per edit).

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
| `instincts.rhai` | Log a (failure → then-succeeded-on-retry) pair and pin the ones that were actually fixed — steers away from repeats without pinning every dead-end failure. |
| `btw.rhai` | Side questions that never enter the main history. |

`failure-diary.rhai`, which pinned *every* recent failure unconditionally (noisier, no signal on whether it was ever fixed), was retired in favor of `instincts.rhai`'s narrower, higher-precision pairing.

## Persistence, memory & handoff

| Extension | What it does |
|-----------|--------------|
| `goal.rhai` | First-class session goals: set, pin, verify, persist. |
| `file-checkpoints.rhai` | VCS-free per-file checkpoints (plain `cp` backups): every edit is restorable, no git required — the option for non-git working directories. |
| `stash-checkpoint.rhai` | Git-based: snapshot via a "dangling" `git stash create` (never stages anything) every turn; `/undo` merges via `git stash apply`. |
| `dmail.rhai` | Model-initiated time travel (D-Mail). |
| `session-handoff.rhai` | Carry context between sessions via `.pirs/handoff.md`. |
| `skill-crystallizer.rhai` | After a successful run, distill what worked into a skill. |

Two packs were retired here. `checkpoint.rhai` (singular): despite the name, it never restored file state at all (just pinned an old text summary of the message log back into context), and it collided with `file-checkpoints.rhai` — both registered the same `/checkpoints`/`/restore` commands into the same `.pirs/checkpoints/log.jsonl` with incompatible schemas, so loading both would have corrupted each other's log. `rollback.rhai`: also git-based (`git add -A && commit-tree`, `/undo` via `git restore`) but `git add -A` stages the user's entire worktree as a side effect of every snapshot — `stash-checkpoint.rhai` does the same job without that side effect, so it's the one that stayed.

`file-checkpoints.rhai` and `stash-checkpoint.rhai` aren't alternatives to each other: `file-checkpoints.rhai` works with no git repo at all. Pick `file-checkpoints.rhai` for a non-git project, `stash-checkpoint.rhai` for a git one.

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
| `word-count.rhai` | Reference example: custom tool, safety hook, loop-behavior hooks. |
