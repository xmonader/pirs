# Extension catalog

Rhai extension packs for `pirs`. Each `.rhai` file registers custom tools,
slash commands, and lifecycle hooks against the same host API. Sources in this
directory are **embedded** into the binary; which ones load for a session is
decided by the active **profile**.

## Profiles select packs

| Profile | How you get it | Packs |
|---------|----------------|-------|
| **`default`** | Implicit (no `--profile`) | `packs: "*"` — full catalog |
| **`weak`** | Explicit `--profile weak` only | smaller stack + junior persona (optional) |
| **custom** | `--profile name` or path | whatever `packs` lists in that script |

Resolution order for a named profile:

1. `.pirs/profiles/<name>.rhai` (project)
2. `~/.pirs/profiles/<name>.rhai` (user)
3. Built-in (`default`, `weak`)

Project/user extension dirs (`.pirs/extensions/`, `~/.pirs/extensions/`) still
load **after** the profile pack set and win on tool-name collisions.

Built-in sources: `crates/pirs-rhai/builtins/default.profile.rhai`,
`weak.profile.rhai`. Catalog order: `pirs_rhai::weak_packs::BUNDLED_ORDER`.

### Custom profile example

```rhai
// ~/.pirs/profiles/minimal.rhai
#{
    name: "minimal",
    // strategy optional — defaults to monolithic
    packs: ["goal", "btw", "guardrails", "auto-checkpoint"],
}
```

```bash
pirs --profile minimal
# or override the built-in default pack set for every session:
# cp minimal.rhai .pirs/profiles/default.rhai
```

`packs` values:

| Value | Meaning |
|-------|---------|
| `"*"` / `"all"` | Full embedded catalog (`BUNDLED_ORDER`) |
| `["goal", "btw", …]` | Those stems, in order |
| omit | Inherit built-in `default` packs (`*`) |
| `[]` | Explicitly no catalog packs (dirs may still load) |

`--no-extensions` disables catalog packs **and** project/user extension dirs.

### `--weak` (CLI composition only)

Composes runtime flags; does **not** change packs (you already get the full
`default` catalog, which includes `weak-model` etc.): `--tool-diet`,
`--sequential`, `max-retries ≥ 3`, one-shot default strategy `plan-exec`,
auto-`--verify` when a test ecosystem is detected. For the optional weak
**role** (persona + smaller pack list), pass `--profile weak` explicitly.

## Load order (full catalog / `packs: "*"`)

Deterministic load order (first → last). Later packs win on **tool name**
collisions.

| Order | Pack | Role |
|------:|------|------|
| 1 | `weak-model.rhai` | Loop thrash detection, verify-after-edit, stop gate, `update_plan` + plan pin |
| 2 | `context-janitor.rhai` | Shrink stale giant tool outputs in outgoing context |
| 3 | `env-doctor.rhai` | Block tools when toolchains are missing |
| 4 | `goal.rhai` | Session goal pin (`[SESSION GOAL]`) |
| 5… | *rest of catalog* | Alphabetical |

Host APIs (after `register_core_host_apis()`): `project_profile(cwd)`,
`project_packages(cwd)`, `skills_index(_)`, `agent_profile(_)` (active
`PIRS_AGENT_PROFILE` name).

## Composition hazards (last-wins / pin channels)

| Hazard | Detail |
|--------|--------|
| **Tool name last-wins** | Two packs that `register_tool` the same name: the **last loaded** implementation runs. Never load a second `update_plan` alongside `weak-model` unless you intend to replace it. |
| **`on_context` rewrites** | Each pack with `on_context` rewrites the full LLM-facing message list in load order. Filters must be **kind-scoped** (strip only your pin), never “drop every `<system-reminder>`”. |
| **Plan pin vs control pins** | `weak-model` de-dupes only `kind=plan`. Host (`pirs_agent::control_pins::preserve_control_pins`) restores protected kinds if a transform drops them. |
| **Goal vs plan formats** | `goal.rhai` pins `[SESSION GOAL]…`; `weak-model` pins `<system-reminder> kind=plan`. Both may be active; they do not share a string prefix. |
| **conductor + weak-model** | `conductor.rhai` deliberately does **not** register `update_plan`. Load weak-model for the tool; conductor for planner/delegate guidance. |
| **Checkpoint packs** | Prefer core `/checkpoint` + `auto-checkpoint.rhai`. `file-checkpoints` / `stash-checkpoint` are alternate layouts. |

## Safety & guardrails

| Extension | What it does |
|-----------|--------------|
| **Rust `--agent-profile`** | Hard gate: `plan` / `accept-edits` / `auto-approve` (not a pack; always on when set). |
| `strict-plan.rhai` | Optional **stricter** plan: blocks web/browser/vision when profile is `plan` (or `PIRS_STRICT_PLAN=1`). |
| `sandbox.rhai` | Overrides `bash` with an OS-level sandbox. |
| `guardrails.rhai` | Hard-blocks catastrophic patterns (`rm -rf /`, `curl \| bash`, force-push, …). |
| `path-guard.rhai` | Blocks commands whose targets resolve outside the working directory. |
| `blast-radius-judge.rhai` | Semantic blast-radius via sub-agent. |
| `diff-shield.rhai` | Merge consecutive same-tool results to compress context. |
| `dirty-guard.rhai` | Commit pre-existing user WIP before the AI edits a file. |
| `env-doctor.rhai` | Block tool calls for missing toolchains, with install hints. |
| `safe-edit.rhai` | Editor-mode edits: a narrow prompt applies a single focused diff. |

## Verification & review

| Extension | What it does |
|-----------|--------------|
| `review-gate.rhai` | Independent sub-agent reviews every diff; can REFUSE completion. |
| `critic.rhai` | Interleaved mid-run critic every N edits. |
| `critic-arena.rhai` | Two models answer the same task; you judge. |
| `shadow-verify.rhai` | Re-run test commands and compare against claimed results. |
| `verify-guard.rhai` | A passing verify that ran ZERO tests does not count. |
| `verify-impact.rhai` | Graph-scoped verification after a successful edit. |
| `mutation-guard.rhai` | Self-verifying codegen via mutation testing. |
| `spec-check.rhai` | Pin `ACCEPTANCE.md`; force a checklist pass before ending. |
| `relay-race.rhai` | Draft → critique → finalize pipeline as a single tool. |

## Cost & budget

| Extension | What it does |
|-----------|--------------|
| `cost-sentinel.rhai` | Cumulative token budget: warn at 50%, hard-stop at cap. |
| `spend-caps.rhai` | Persistent USD spending caps (daily/monthly). |

## Multi-agent & orchestration

| Extension | What it does |
|-----------|--------------|
| `conductor.rhai` | Strong-planner / weak-executor guidance pack. |
| `weak-model.rhai` | Loop hardening for weaker models. |
| `subagents.rhai` | Named sub-agents from `.pirs/agents/*.md`. |
| `fork.rhai` | Cache-warm delegates. |
| `workflow.rhai` | Rerunnable multi-agent workflow. |
| `swarm.rhai` | Work-queue discipline over the hive. |
| `hive-note.rhai` | Shared blackboard for multiple pirs instances. |

## Context management

| Extension | What it does |
|-----------|--------------|
| `context-janitor.rhai` | Shrink stale giant tool outputs. |
| `semantic-bookmarks.rhai` | Model-managed pinned notes at the context tail. |
| `repo-pulse.rhai` | Fresh repo-state pin (branch, dirty files). |
| `instincts.rhai` | Log (failure → fix) pairs and pin the ones that worked. |
| `btw.rhai` | Side questions that never enter the main history. |

## Persistence, memory & handoff

| Extension | What it does |
|-----------|--------------|
| `goal.rhai` | First-class session goals: set, pin, verify, persist. |
| **Core `checkpoint` tool / `/checkpoint`** | Default recoverability (not a pack). |
| `auto-checkpoint.rhai` | After successful mutate tools, call core `checkpoint_create`. |
| `file-checkpoints.rhai` | Optional VCS-free per-file backups + pack `/restore`. |
| `stash-checkpoint.rhai` | Optional git dangling-stash per turn + pack `/undo`. |
| `dmail.rhai` | Model-initiated time travel (D-Mail). |
| `session-handoff.rhai` | Carry context between sessions via `.pirs/handoff.md`. |
| `skill-crystallizer.rhai` | Distill what worked into a skill. |

## Provenance & audit

| Extension | What it does |
|-----------|--------------|
| `blame.rhai` | Line-level provenance via git notes. |
| `audit-log.rhai` | Full tool call/result log to `~/.pirs/audit.jsonl`. |
| `runs.rhai` | Durable run records under `~/.pirs/runs/`. |
| `telemetry.rhai` | Metadata-only usage stats (never prompt/tool content). |

## Authoring & misc

| Extension | What it does |
|-----------|--------------|
| `chapter-spine.rhai` | One-line chapter titles for long sessions. |
| `web-tools.rhai` | `web_fetch` and `web_search` via curl. |
| `word-count.rhai` | Reference example pack. |
| `browser-cdp-workflow.rhai` | CDP multi-step recipes / thrash steering. |
| `project-discipline.rhai` | Steer toward shared `project` tool. |
| `session-discipline.rhai` | Steer `todo` / `ask_user` usage. |
