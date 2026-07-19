# pirs-bench — benchmark-attack harness (final plan)

A complete implementation plan for a harness that solves SWE-bench-style tasks
(repo + issue → verified patch) on top of pirs. The model's capability is fixed;
every point comes from **lowering the model's error rate and making feedback
high-signal**, while staying **bounded, verified, and honest** so we never submit
a false green.

**Design constraint: keep the Rust core small.** Push as much logic as possible
into **Rhai** (`pirs-rhai`), and keep in compiled Rust only the primitives and the
invariant-critical gate. The split has one governing principle:

> **Rhai owns what is self-correcting; Rust owns what fails silently or
> catastrophically.** A wrong Rhai detector fails its probe; a wrong Rhai policy
> wastes one iteration caught by the gate. But the verification gate, test
> execution, and snapshotting cannot be allowed to fail silently — those stay in
> Rust. Probe-confirm and the gate are the safety nets that make Rhai heuristics
> safe to get wrong: they degrade to *slower*, never *wrong*.

---

## 1. Invariants (the score-protectors — enforced in Rust, never overridable)

1. **Reproduce-before-fix, verify-before-done.**
2. **0 tests collected = FAILURE, never pass.** Anchor on the named target
   transitioning, not exit status.
3. **Differential, not absolute.** Repos aren't green at checkout. Verify vs a
   captured baseline: targets go red→green; nothing green regresses; pre-existing
   reds are out of scope.
4. **Bounded input.** Query don't read; symbol-slice not whole-file; grep-extract
   docs not walk them; per-file byte caps; broad sweeps in subagents.
5. **Bounded cost.** Concentric test rings; minimum delta-proving set; full suite
   ≤1×/task; per-ring wall-clock budgets.
6. **Verify before trust.** Discovered commands probe-confirmed; red→green flips
   re-confirmed on a second run.
7. **Degrade safe, fail loud.** A fallback ladder that never dead-ends or goes
   silent. Honest "couldn't run X because Y" over confident wrong.
8. **Minimal diff.** Touch only what the task requires.
9. **Measure where you lose.** Every abort records a typed `FailBucket`.

These live in **trusted Rust core**. No project-tier Rhai script can weaken them
(see §9 security).

---

## 2. The Rust / Rhai split

### Rust core (small, verified, invariant-owning)

- **`primitives`** — host functions exposed to Rhai. The only things that touch
  subprocesses, fs, tree-sitter, the sandbox, or model APIs:
  - `run_tests(spec, test_ids, timeout) -> Map<TestId, Outcome>`
  - `snapshot(test_ids, ring) -> Snapshot`  (SHA-keyed cache)
  - `probe(cmd) -> ProbeResult`             (list/collect-only: count + stderr)
  - `apply_edit(edits) -> EditResult`       (ast_edit: validated + rollback)
  - `graph_defs_refs(seeds)`, `graph_affected_tests(files)`, `lsp_refs(sym)`
  - `read_slice(path, symbol)`, `grep_extract(dirs, pattern, caps)`
  - `sandbox_run(cmd, timeout)`
  - `ask_model(brief_json, schema_json, tier) -> Value`  (agent + forced schema)
- **`gate::verify(...)`** — THE invariant. Rust, unit-tested, not scriptable.
- **`driver`** — the skeleton state machine. Guarantees phase order, guarantees
  the gate runs and its verdict is authoritative, records attribution, and calls
  Rhai policy hooks at each decision point. **You cannot reach `Done` without a
  `Done` verdict from `gate`** — the skeleton enforces this in Rust.
- **`types`** — `TestOutcome`, `Snapshot`, `Verdict`, `FailBucket`, `RunnerSpec`,
  `Ring` (serde ⇄ Rhai maps).

### Rhai extension layer (bundled/trusted tier — "as much as possible")

- **`detectors/*.rhai`** — one per ecosystem: `detect(root) -> [RunnerSpec]`,
  built from `grep_extract`/`read_slice`. Go/Rust/pytest bundled; add a language
  = drop in a script, no recompile. Self-correcting via `probe`.
- **`policy.rhai`** — phase-transition policy, ring escalation, per-ring budgets,
  trigger sets. Pure decision logic over serialized state; calls no unsafe
  primitive.
- **`orchestration.rhai`** — planner/steerer: builds `PlanBrief`, holds the
  schemas, decides trigger conditions, calls `ask_model`. A/B-gated (§8).
- **`mining.rhai`** — CI/doc grep patterns + install-step extraction.
- **`report.rhai`** — attribution formatting.

Everything that is *heuristic, per-ecosystem, prompt-shaped, or policy* is Rhai
and hot-swappable without recompiling. Everything whose failure is silent stays
in the ~small Rust core.

---

## 3. Data model

```rust
struct TaskContext {
    repo_root:  PathBuf,
    issue_text: String,
    targets:    Vec<TestId>,   // FAIL_TO_PASS; empty in generic mode → derive from traceback
    keep_green: Vec<TestId>,   // PASS_TO_PASS
    runner:     Option<RunnerSpec>,
    baseline:   HashMap<Ring, Snapshot>,
    edited:     Vec<PathBuf>,
    phase:      Phase,
    log:        Vec<PhaseEvent>,
}
struct RunnerSpec {            // produced by a Rhai detector, consumed by Rust primitives
    framework: String, install: Vec<String>, list_cmd: String,
    run_one: String, run_scope: String, run_all: String, parallel: bool,
}
enum Ring { Inner, Scoped, Full }
enum TestOutcome { Pass, Fail, Errored, NotCollected }
struct Snapshot { states: HashMap<TestId, TestOutcome>, build_ok: bool, runs: u8 }
enum Phase { Bootstrap, Baseline, Reproduce, Localize, Fix, Verify, Refine, Select, Done }
enum FailBucket { BaselineUnusable, RunnerUndetected, ReproFailed, LocalizeMiss,
                  FixNoFlip, Regressed, Flaky, Timeout, EnvSetup }
enum Outcome { Solved, AcceptedScopedOnly, Failed(FailBucket) }
```

---

## 4. The per-task state machine

Rust `driver` skeleton owns phase order + gate enforcement; `policy.rhai` decides
transitions/budgets/triggers within it.

| Phase | Goal | Success gate | On failure |
|---|---|---|---|
| **0 Bootstrap** | Env installs; can run *any* test | `list_cmd` clean, ≥1 collected | retry doc-mined install → abort `EnvSetup` |
| **0.5 Baseline** | Capture pre-edit state (targets now, scoped later) | stable across 2 runs | abort `BaselineUnusable` |
| **1 Reproduce** | Targets fail *for the issue's reason* | every target ∈ {Fail, Errored} | `ReproFailed` |
| **2 Localize** | Small candidate edit-site set | ≥1 site, bounded reads | widen walk → hand to model |
| **3 Fix** | Minimal validated edits | `apply_edit` ok | roll back, next candidate |
| **4 Verify** | Differential green (ring-scoped) | targets flip red→green (2×); no baseline-green regresses | `FixNoFlip`/`Regressed` → Refine |
| **5 Refine** | Iterate on exact error | ≤N attempts, one hypothesis each | escalate → Select or report |
| **6 Select** | Best candidate | judge + test-pass | submit minimal diff |

Reproduce/fix/refine stay in the **Inner** ring (targets, seconds). **Scoped**
gates acceptance. **Full** runs ≤1× at Done.

---

## 5. Runner discovery (Rhai detectors, ranked, probe-confirmed)

`detectors/*.rhai` each emit candidate `RunnerSpec`s in **trust order**; the Rust
driver `probe`s them and takes the first that lists ≥1 test:

1. **CI oracle** — `.github/workflows/*.yml`, `.gitlab-ci.yml`, `tox.ini` `run:` line. Executed truth.
2. **Manifests** — `package.json:scripts.test`, `Makefile`, `pyproject.toml`, `Cargo.toml`, `go.mod`, `pom.xml`.
3. **Docs (grep-extract, never read-whole)** — `README*`/`CONTRIBUTING*`/`docs/dev*`, depth+match capped, mined mainly for **install/env steps**.
4. **Structural inference** — `*_test.go`→GoTest, `conftest.py`→Pytest, etc.

A failing probe's **stderr is surfaced as an env-repair signal**. Prefer parallel
runners (`cargo nextest`, `pytest -n auto`, `-j`). **CI-slow ≠ you-slow:** CI is a
full matrix; you run the minimum delta-proving set once, in one env.

Bundle Go + Rust + pytest first; JVM/JS next; C/C++ leans hardest on the CI oracle.

---

## 6. Cost model — concentric rings (Rust primitives, Rhai policy)

| Ring | Test set | Run when | Cost |
|---|---|---|---|
| **Inner** | Targets only | every reproduce/fix/refine iteration | seconds |
| **Scoped** | `graph_affected_tests(edited)` — transitive importers | before accepting a candidate | seconds–min |
| **Full** | All `keep_green` | once, at Done, if budget allows | min+ |

Baseline captured per-ring, cached by `(test_id, base_sha)`. Never baseline tests
you won't run; never re-run an unchanged ring. Per-ring budgets in `policy.rhai`;
on Full-ring timeout → `AcceptedScopedOnly` (flagged) — the grader runs full
`keep_green` anyway, so the internal Full run is an optimization, not the final word.

---

## 7. The verification gate (Rust, load-bearing, build first)

```rust
enum Verdict { Done, NotYet(TestId), Regressed(TestId), TargetNotCollected(TestId), Flaky(TestId) }

fn verify(ctx: &TaskContext, ring: Ring) -> Verdict {
    let base  = &ctx.baseline[&ring];
    let scope = test_set_for(ctx, ring);         // Inner=targets, Scoped=affected, Full=keep_green
    let post  = run_tests(&ctx.runner, &scope);

    for t in &ctx.targets {                       // (1) flip red->green; NotCollected = FAILURE
        match (base.states.get(t), post.states.get(t)) {
            (Some(Fail | Errored), Some(Pass)) => {}
            (_, Some(NotCollected) | None)     => return TargetNotCollected(t.clone()),
            _                                  => return NotYet(t.clone()),
        }
    }
    for t in &scope {                             // (2) no baseline-green regresses; reds ignored
        if base.states.get(t) == Some(&Pass) && post.states.get(t) != Some(&Pass) {
            return Regressed(t.clone());
        }
    }
    let post2 = run_tests(&ctx.runner, &ctx.targets);  // (3) re-confirm flips (flaky guard)
    for t in &ctx.targets { if post2.states.get(t) != Some(&Pass) { return Flaky(t.clone()); } }
    Verdict::Done
}
```

Never claim `Done` without watching the named targets flip **twice**. This is not
scriptable.

---

## 8. Strong-model orchestration layer (Rhai prompts/triggers over Rust `ask_model`)

Optional, A/B-flagged. **Principle: the strong model operates on structured state
at defined checkpoints and emits typed decisions the harness validates and
executes; it never touches files or control flow, and cannot override an
invariant.**

- **Planner** (`orchestration.rhai`): at hard checkpoints — *after Localize*
  (which site / what approach), *on Refine escalation* (fresh hypothesis after k
  failures), *ambiguous env/runner* — build a grounded `PlanBrief` (issue +
  candidate slices + failing output + baseline delta + attempts) and
  `ask_model(brief, PLAN_SCHEMA, Strong)`. Grounded on **harvested evidence**, not
  raw repo.
- **Steerer**: trigger-based (k failures / stall / low confidence), writes typed
  `Hint`s to the steering channel the agent loop already drains **between turns**.

**Guardrails against degradation** (it *can* degrade — hold these):
- **Don't downgrade the executor to justify the split.** Strong-solo is the
  baseline to beat; burden of proof is on the layer.
- **Hints are advisory; invariants are code.** A hint may redirect attention;
  it may never skip verification or mark done.
- **The gate makes bad planning cheap:** a bad plan/hint costs one iteration,
  never a wrong result. Degradation shows up as latency, not correctness.
- **Turn it on only where the histogram (§10) shows judgment failures**
  (`LocalizeMiss`, `FixNoFlip`) — not everywhere, not for `EnvSetup`.
- **A/B it, don't reason about it.** Ship only if solve-rate beats strong-solo at
  acceptable cost. Every added tier pays for its seam or is reverted.

---

## 9. Security — bench mode disables project-tier Rhai

`pirs-rhai` trust-gates project scripts, but in bench mode the **task repo is
untrusted** and could ship `.rhai` files (or `.mcp.json`, hooks) to subvert the
harness. Therefore:

- In bench mode, **load only bundled/home-trusted scripts; never load Rhai,
  MCP, or hooks from the task repo.** The harness's own logic must not be
  extensible by the code under test.
- Invariant-critical logic (gate, driver skeleton, primitives) is Rust and not
  script-overridable at any tier.
- All task commands run under the existing sandbox; doc-mined commands are
  constrained to a test-invocation shape before execution.

(Reuses the trust/sandbox hardening already in `pirs-rhai`, `pirs-mcp`, `pirs-tools`.)

---

## 10. Instrumentation (Rust events + `report.rhai` formatting)

Every task emits a `PhaseEvent` trail; every abort records a `FailBucket`. The
histogram is the roadmap and the A/B judge for §8.

```
Solved: X%   AcceptedScopedOnly: __
BaselineUnusable __  RunnerUndetected __  ReproFailed __  LocalizeMiss __
FixNoFlip __  Regressed __  Flaky __  Timeout __  EnvSetup __
```
Prior: `BaselineUnusable` + `EnvSetup` dominate early and are under-anticipated.

---

## 11. Invariant → enforcement point

| Invariant | Enforced in |
|---|---|
| Reproduce-before-fix; verify-before-done; 0-collected=fail | Rust `gate` + `driver` skeleton |
| Differential (baseline, not green) | Rust `gate` + per-ring `Snapshot` |
| Bounded input | `read_slice`/`grep_extract` caps, `scratch_dir`, read/grep byte caps (shipped) |
| Bounded cost | rings §6, budgets in `policy.rhai`, SHA baseline cache |
| Verify before trust | `probe` §5 + flip re-confirm §7 |
| Minimal diff / reds out of scope | targets-only in `gate`; scoped `apply_edit` |
| Degrade safe, fail loud | driver degrade paths → `FailBucket`/`AcceptedScopedOnly` |
| No untrusted extension | bench mode blocks project-tier Rhai/MCP §9 |
| Measure | Rust `PhaseEvent` + `report.rhai` |

---

## 12. Build order — milestones & acceptance

- **M1 — Rust gate + attribution. ✅ DONE.** `TestOutcome`/`Snapshot`/`Verdict`/
  `FailBucket`/`Outcome`, `gate::{provisional,confirm_flips,evaluate}`
  (differential + anti-false-green + flaky-reconfirm), `Attribution` histogram.
  Accepted: good flip→`Done`; still-red→`NotYet`; deleted-target→
  `TargetNotCollected`; neighbor-break→`Regressed`; unstable flip→`Flaky`;
  pre-existing red ignored. All unit-tested (`crates/pirs-bench`).
- **M2 — Rust primitives. ✅ DONE (Rhai host binding pending).** `verify`
  orchestrator with `TestRunner` seam + lazy confirmation run; JUnit parser +
  node-id matcher (unreported id ⇒ `NotCollected`); `CommandRunner` (subprocess
  + process-group timeout); `probe` (collect-only confirm, keeps stderr as
  repair signal); shared `proc::run_capture` (deadlock-safe capture + group
  kill). *Remaining:* expose these to Rhai + bench-mode script isolation (§9).
- **M3 — Rhai detectors + discovery. ✅ DONE (CI oracle pending).** Read-only
  `DetectorHost` (root-confined `file_read`/`path_exists`/`read_dir`); bundled
  pytest/go/rust detectors compiled in via `include_str!`; `discover` probes
  candidates in trust order and returns the first confirmed, keeping the last
  failing stderr as an env-repair hint. Bench-mode isolation is structural (host
  loads only trusted scripts, cannot exec/write). *Remaining:* CI-config oracle
  detector; parallel probing.
- **M4 — Bootstrap + baseline cache. ✅ DONE (`policy.rhai` pending).**
  `bootstrap` (best-effort install, probe-gated, repair hint on failure);
  `capture_stable` (twice-agree) + `capture_stable_cached` over a SHA-keyed
  `BaselineCache` (atomic-persisted, corrupt-tolerant, reused across
  attempts/tasks); `targets_reproduce` gate. *Remaining:* phase/ring/budget
  `policy.rhai`.
- **M5 — Localization + scoped ring + driver. ✅ DONE (LSP path pending).**
  `parse_traceback` (Python/pytest/Rust/Go) → `rank_candidates` (graph-backed:
  project>vendored, source>test, symbol-confirmed ×1.5) → `scoped_tests` via
  `Graph::affected_tests`; `driver::run_task` state machine that *structurally*
  makes `Solved` require a gate `Done`. Concentric rings: refinement verifies the
  Inner ring (targets) only; the regression ring runs at most once, after a flip.
  *Remaining:* LSP-based localization as a second signal alongside the graph.
- **Capstone — end-to-end harness. ✅ DONE + PROVEN.** `run_instance` composes
  discover → bootstrap → runner → cached baseline → reproduce → fix/verify, each
  failure mapped to a typed `FailBucket`. A real pytest e2e (`tests/e2e_pytest`)
  fixes a real bug via a real file edit through the whole pipeline, and asserts an
  unpatched bug is never a false pass. Fixed en route: `python3`-only interpreter
  resolution; per-framework `test_join` (Go `-run` regex alternation).
- **M6 — Strong-model planner/steerer. ✅ DONE (Rhai policy pending).**
  `ModelOracle` (`ask_model`) trait; `plan_next` returns a `PlanDecision` that is
  hard-validated to a reorder/filter of the real candidate set (invented paths
  dropped, omitted candidates re-appended, all failures degrade to deterministic
  order); `steer_hint` advisory-only. A/B-honest: oracle-disabled path is
  byte-for-byte deterministic. *Remaining:* `orchestration.rhai` policy layer;
  live A/B measurement (*Accept:* beats strong-solo at acceptable cost, else
  reverted).

---

## 13. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Env bootstrap dominates failures | M4 first-class; doc-mine env steps; precise `EnvSetup` aborts |
| Slow CI / suite | rings; parallel runners; Full ≤1×/task; budgets → `AcceptedScopedOnly` |
| Flaky baseline/targets | two-run flip confirmation §7; stable-baseline requirement |
| Broken build at checkout | `BaselineUnusable` abort, don't thrash |
| Overfit to visible target | scoped regression ring always run before accept |
| Scoped ring misses a regression | generous transitive-importer set + Full backstop; flag when Full skipped |
| Untrusted repo ships `.rhai`/`.mcp.json`/hooks | bench mode loads trusted scripts only §9 |
| Rhai heuristic wrong | self-correcting: probe fails or gate wastes one iteration — never wrong |
| Orchestration layer degrades quality | strong-solo baseline; A/B gate; hints advisory; gate caps downside to latency §8 |
| Context rot on big repos | bounded retrieval + subagent sweeps |

---

**Critical path: M1 (Rust gate) first, then the Rhai host API (M2).** The gate is
what makes every later phase — and every Rhai heuristic — trustworthy: it turns
detector mistakes and bad plans into *slower*, never *wrong*. Keep the Rust core
to the primitives + gate + driver skeleton; everything heuristic, per-ecosystem,
and prompt-shaped lives in Rhai.
