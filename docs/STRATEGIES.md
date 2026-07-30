# Strategies

Multi-phase loop policies that shape how `pirs` (and `pirs-bench`) run an agent
on a task. **Mechanism** lives in Rust (`pirs-agent`); **policy** lives in Rhai
scripts under `crates/pirs-rhai/builtins/`.

## How strategies load

Resolution order for `--strategy NAME`:

1. `.pirs/strategies/<name>.rhai` (project)
2. `~/.pirs/strategies/<name>.rhai` (user)
3. Built-in embedded script (same stem)

Aliases are normalized in `pirs_rhai::builtins::canonicalize_name` before
lookup (e.g. `advisor` → `weak-drive`).

Built-ins are compiled into the binary with `include_str!` and parse-checked
in unit tests (`all_builtins_parse…`). A parse failure in a shipped script is
a panic on first use — caught by CI-style `cargo test -p pirs-rhai`.

## Catalog

| Name | Aliases | Phases (conceptually) | Multi-model |
|------|---------|----------------------|-------------|
| `monolithic` | | Single growing loop | `--model` only |
| `plan-exec` | | Readonly plan → full exec | `--plan-model` on plan |
| `plan-critic-exec` | `plan-exec-critic` | Plan → critic → exec | plan/critic on plan-model |
| `wide-plan-exec` | | Wide/fan plan variants | secondary |
| `plan-exec-weak` | | Weak-oriented plan-exec | secondary |
| `spark-ember` | `dual`, `soul-dual` | Parallel spark explore → ember code | spark↔plan, ember↔model |
| **`weak-drive`** | `advisor`, `weak-strong`, `advise-exec`, `weak_drive`, `weak-advisor` | Strong plan → weak exec → strong review → weak fixup | **required** for hybrid economics |

Primary product set (front door): `monolithic`, `plan-exec`, `plan-critic-exec`.
`weak-drive` and `spark-ember` are first-class built-ins for dual-mode / hybrid
experiments.

## CLI shapes

```bash
# Mono baseline
pirs --model deepseek-v4-flash --strategy monolithic "…"

# Strong plan / weak exec (classic)
pirs --model deepseek-v4-flash --plan-model deepseek-v4-pro \
  --strategy plan-exec "…"

# weak-drive hybrid (recommended multi-model shape)
pirs --model deepseek-v4-flash --plan-model deepseek-v4-pro \
  --strategy weak-drive "…"

# Optional verify loop (re-plan on gate fail)
pirs --model … --plan-model … --strategy plan-exec \
  --verify "pytest -q" --max-attempts 3 "…"

# Weak runtime composition (tool-diet, retries, default plan-exec one-shot)
pirs --weak --plan-model deepseek-v4-pro "…"
```

When `--plan-model` ≠ `--model`, session end / one-shot footers include a
**by role** token split (planner vs executor). See [MODELS.md](MODELS.md) and
[hybrid-model-economics.md](hybrid-model-economics.md).

## Script shape

A strategy file evaluates to a map:

```rhai
#{
    name: "example",
    persist: false,       // optional
    hybrid: true,         // optional — agentpy thrash→advisor on full phases
    phases: [
        #{
            scope: "readonly",   // or "full"
            system: "…",
            prompt: "… {issue} {targets} {prev} {prev_0} {verdict}",
            // skip_if_prev_prefix: "APPROVE",  // free skip when prev starts with this
        },
        // …
    ],
}
```

### Template variables

| Token | Meaning |
|-------|---------|
| `{issue}` | Task / problem statement |
| `{targets}` | Tests / success criteria text |
| `{prev}` | Previous phase output |
| `{prev_0}` | Phase 0 output (kept for later phases — critical for review) |
| `{verdict}` | Prior attempt failure preamble when `--verify` retries |

### Scope and model pinning

- `readonly` phases get the plan-model when `--plan-model` is set
  (`pin_plan_model` in the runner).
- `full` phases use `--model` and the full tool set (subject to diet/profile).

## weak-drive (agentpy hybrid as a pirs strategy)

**Intent:** cheap model drives tools; strong model only at scheduled plan/review
checkpoints — and (on the product path) when thrash fires mid-loop.

Source of truth: [`crates/pirs-rhai/builtins/weak-drive.rhai`](../crates/pirs-rhai/builtins/weak-drive.rhai).

| # | Role | Scope | Model flag | Notes |
|---|------|-------|------------|-------|
| 0 | Strong plan | readonly | `--plan-model` | Self-contained plan for a weaker executor |
| 1 | Weak exec | full | `--model` | Hybrid thrash→advisor + `ask_advisor` when hybrid on |
| 2 | Strong review | readonly | `--plan-model` | `APPROVE` or `REVISE`; sees `{prev_0}` plan + `{prev}` exec |
| 3 | Weak fixup | full | `--model` | `skip_if_prev_prefix: "APPROVE"` — free skip on approve |

### Hybrid flag (`hybrid: true`)

Rust: [`crates/pirs-agent/src/hybrid.rs`](../crates/pirs-agent/src/hybrid.rs),
wired from [`crates/pirs/src/turn.rs`](../crates/pirs/src/turn.rs).

When the strategy sets `hybrid: true` **and** the run has `--plan-model`:

1. **Thrash → advisor** — loop/mistake thrash injects strong-model guidance and
   *continues* the weak loop (instead of hard-stop).
2. **Staged escalate** — short summary first, fuller trajectory later; budgeted
   (default max advisor calls).
3. **`ask_advisor` tool** — weak may request guidance; thrash remains the
   authority for “stuck,” not self-report.

Without `--plan-model`, hybrid degrades: thrash hard-stops; log line warns.

The first readonly phase text is stored as the hybrid **plan** for advisor
context (`set_plan` after phase 0).

### Product vs bench

| Path | Phases | Dual models | Mid-loop thrash→advisor |
|------|--------|-------------|-------------------------|
| Product `pirs` | yes | yes (`--plan-model`) | **yes** when `hybrid: true` |
| `pirs-bench` weak-drive campaigns | yes | yes | phase-only unless runner wires hybrid the same way |

Strict SWE Lite-50 (2026-07-30): **34/50** with
`--model deepseek-v4-flash --plan-model deepseek-v4-pro --strategy weak-drive`.
Reports: `qa/bench-swebench-5x5/results_deepseek_v4_flash_strict_weak_drive_fifty/`.

Do **not** reintroduce agentpy as a second product path. New hybrid behavior
belongs in `hybrid.rs` + strategy scripts.

## spark-ember

Soulrs dual-mode analogue: parallel read-only **spark** explore fan, then
**ember** coding. Pair `--plan-model` with spark and `--model` with ember.
Aliases: `dual`, `soul-dual`.

## Writing or overriding a strategy

1. Copy a builtin from `crates/pirs-rhai/builtins/`.
2. Drop override at `.pirs/strategies/<name>.rhai` (project) for local experiments.
3. For shipping: edit the builtin + ensure `builtins.rs` registry/aliases + tests.
4. Keep prompts self-contained for weak executors (no “see above” without
   `{prev}` / `{prev_0}`).
5. Prefer REVISE over false APPROVE on review phases.

### Checklist for hybrid strategies

- [ ] Readonly plan phase first (or clear plan capture rule)
- [ ] Full exec phase(s) that should thrash-escalate
- [ ] Review retains plan via `{prev_0}`
- [ ] APPROVE path free-skips fixup when appropriate
- [ ] `hybrid: true` only if product thrash→advisor is desired
- [ ] Document in this file + `AGENTS.md` table

## Related

- Measured cost/quality matrices: [hybrid-model-economics.md](hybrid-model-economics.md)
- Model pins: [MODELS.md](MODELS.md)
- SWE campaigns: [SWE-QA.md](SWE-QA.md)
- Agent map: [../AGENTS.md](../AGENTS.md)
