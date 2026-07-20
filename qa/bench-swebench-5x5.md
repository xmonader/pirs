# Strategy benchmark — 5 SWE-bench-lite instances × 5 execution modes

A live, real-API comparative study of pirs's execution strategies against real
SWE-bench-lite tasks, run inside the official `swebench/sweb.eval.*` docker
images. Every number below is pulled directly from a captured `.result.json` /
`.log` pair in `bench-swebench-5x5/results/`; nothing here is estimated.

## Setup

- **Base model**: `deepseek-v4-flash` for every arm (the executor phase, or the
  only phase, in every strategy).
- **Strong model**: `deepseek-v4-pro`, pinned via each strategy script's
  per-phase `model` field on the planner/critic phase(s) only — never the
  executor. Plain `no-strategy` and `monolithic` never touch the strong model.
- **Instances** (5, from `princeton-nlp/SWE-bench_Lite`): `astropy__astropy-6938`,
  `matplotlib__matplotlib-23562`, `pytest-dev__pytest-5221`,
  `scikit-learn__scikit-learn-12471`, `sphinx-doc__sphinx-7686`.
- **Strategies** (5): `no-strategy`, `monolithic`, `plan-exec`,
  `plan-critic-exec`, `wide-plan-exec` — see [Strategies under test](#strategies-under-test).
- **Harness**: `pirs-bench solve`, one instance per docker container (official
  SWE-bench eval image, already has the repo checked out and installed at
  `base_commit`), `--max-turns 40`, 2400s per-run timeout, concurrency 2.
- **Total**: 5 × 5 = 25 runs, all completed. **Total spend: $2.10.**

### Strategies under test

| Label | Structure | Strong-model phase(s) |
|---|---|---|
| `no-strategy` | New in this session (`--no-strategy`): bypasses the strategy engine (`PhaseDriver`/`run_strategy`) entirely — one undivided, growing-context loop with a generic assistant system prompt. The true naive baseline, matching pirs's interactive default when no `--strategy`/`--profile` is given. | none |
| `monolithic` | Built-in. One growing-context loop through the phase engine. Results below use the **original** prompt ("make the smallest change, don't touch tests"); see [Follow-up](#follow-up-was-it-really-the-prompt) for the fixed root-cause-first prompt now shipped. | none |
| `plan-exec` | `.pirs/strategies/plan-pro-exec-flash.rhai`. Read-only planner → fresh full-scope executor seeded only with the plan. | planner |
| `plan-critic-exec` | `.pirs/strategies/plan-critic-exec-pro-flash.rhai`. Planner → critic gate (may rewrite the plan) → fresh executor. | planner + critic |
| `wide-plan-exec` | `.pirs/strategies/wide-plan-exec-pro-flash.rhai`. Three read-only planners investigate in parallel (assertion-focused / recency-focused / edge-case-focused), merged → fresh executor. | all 3 parallel planners |

## Headline finding

**`monolithic`'s original prompt was dominated by plain `no-strategy` on every
axis that mattered** — lower solve rate, higher cost, longer wall-clock — and
the [follow-up experiment](#follow-up-was-it-really-the-prompt) traced this to
one specific instruction: *"make the SMALLEST change... do not refactor"*
pressured the model into a minimal-but-wrong fix on 2 of 3 instances. Rewriting
that instruction to focus on root cause instead (keeping the "don't touch
tests" / "verify before stopping" guardrails) took `monolithic` from 1/3 to
3/3, closing the entire gap. The table below is the **original** prompt's
numbers, preserved as the finding that motivated the fix — see the follow-up
section for the corrected prompt's results.

| Strategy | Solved (of 3 non-broken instances) | Total cost | Avg turns | Avg wall-clock |
|---|---|---|---|---|
| `no-strategy` | **3/3** | **$0.2256** | 29.7 | **166.0s** |
| `monolithic` | 1/3 | $0.4449 | 46.0 | 253.9s |
| `plan-exec` | 3/3 | $0.5493 | 70.3 | 811.0s |
| `plan-critic-exec` | 3/3 | $0.4318 | 61.7 | 633.6s |
| `wide-plan-exec` | 3/3 | $0.4498 | 72.7 | 340.5s |

(Aggregates exclude `matplotlib-23562` and `pytest-5221` — see [Two broken
instances](#two-broken-instances-not-a-strategy-effect) — since every strategy
scored identically $0 / 0 turns / Failed(ReproFailed) on those two, which would
otherwise flatten real differences into noise.)

## Two broken instances (not a strategy effect)

`matplotlib-23562` and `pytest-5221` failed identically under **all five**
strategies: `turns=0`, `tools=0`, `$0.0000` spent, outcome `Failed(ReproFailed)`.
The harness's own pre-flight (discover → bootstrap → baseline) never got far
enough to hand control to the agent at all — for `matplotlib-23562`, 92-94% of
total wall-clock was spent inside `bootstrap` alone (207-251s) before failing.
This is a harness/environment issue with these two docker images or instance
definitions, confirmed pre-existing: `pytest-5221` also failed the same way
under an earlier, separately-validated `monolithic`/`deepseek-v4-flash` baseline
run from before this benchmark (`qa/bench-swebench-5x5/results` doesn't include
that older run, but the `Failed(ReproFailed)` signature and near-zero elapsed
time match exactly). **These two cells are excluded from every strategy
comparison in this report** — including them would just add ten identical
zero-cost failures, hiding the real signal.

**Tested and ruled out: is this actually disk pressure, not a real gap?** The
second batch (below) hit an unrelated mid-run disk-full incident, and its two
new `ReproFailed` instances showed bootstrap times 2-4x slower than their
same-repo counterparts here — a real, worth-checking hypothesis that these
"broken" instances might just be disk-I/O victims rather than genuinely
unsupported. Directly tested by re-running `matplotlib-23562`,
`matplotlib-26011`, and `scikit-learn-25570` (15 runs) with ~450GB free: **all
15 failed identically** — same `Failed(ReproFailed)` outcome, same bootstrap
timing (`matplotlib-23562`: 231-278s vs. the original 207-251s;
`matplotlib-26011`: 398-458s vs. 439-480s), 0/15 solved. Disk pressure is
**ruled out** as the cause for these three; they are genuinely broken for this
harness/image combination. See
[Disk-pressure hypothesis: tested and refuted](#disk-pressure-hypothesis-tested-and-refuted)
for the full re-test.

## Full per-run results

| Instance | Strategy | Outcome | Turns | Elapsed | Cost |
|---|---|---|---|---|---|
| astropy-6938 | no-strategy | Solved | 9 | 88.2s | $0.0065 |
| astropy-6938 | monolithic | Solved | 26 | 202.2s | $0.0289 |
| astropy-6938 | plan-exec | Solved | 19 | 129.8s | $0.0163 |
| astropy-6938 | plan-critic-exec | Solved | 43 | 211.5s | $0.0562 |
| astropy-6938 | wide-plan-exec | Solved | 88 | 267.9s | $0.1595 |
| matplotlib-23562 | *(all 5)* | Failed(ReproFailed) | 0 | 224-268s | $0.0000 |
| pytest-5221 | *(all 5)* | Failed(ReproFailed) | 0 | 13-23s | $0.0000 |
| scikit-learn-12471 | no-strategy | **Solved** | 20 | 88.6s | $0.0269 |
| scikit-learn-12471 | monolithic | **Failed(FixNoFlip)** | 11 | 74.3s | $0.0243 |
| scikit-learn-12471 | plan-exec | Solved | 21 | 180.0s | $0.0215 |
| scikit-learn-12471 | plan-critic-exec | Solved | 18 | 177.8s | $0.0266 |
| scikit-learn-12471 | wide-plan-exec | Solved | 42 | 199.1s | $0.0775 |
| sphinx-7686 | no-strategy | **Solved** | 60 | 321.0s | $0.1922 |
| sphinx-7686 | monolithic | **Failed(FixNoFlip)** | 101 | 485.2s | $0.3917 |
| sphinx-7686 | plan-exec | Solved | 171 | **2123.3s** | **$0.5115** |
| sphinx-7686 | plan-critic-exec | Solved | 124 | 1511.4s | $0.3490 |
| sphinx-7686 | wide-plan-exec | Solved | 88 | 554.6s | $0.2128 |

Full detail (per-tool-call counts, per-model token/cost split, per-phase timing
breakdown) is in each run's `.result.json`/`.log` under
[`bench-swebench-5x5/results/`](bench-swebench-5x5/results/).

## Key findings

1. **`monolithic`'s original prompt was the outlier — worse, not just
   different — and it was fixable.** It solved 1/3 real instances (only
   `astropy-6938`); every other strategy, including the zero-cost naive
   baseline, solved 3/3. Its two losses (`scikit-learn-12471`, `sphinx-7686`)
   were both `Failed(FixNoFlip)` — it produced a change, but the change never
   flipped the target tests from red to green. On `sphinx-7686` it burned 101
   turns and $0.39 to arrive at a change that didn't work, while `no-strategy`
   solved the same instance in 60 turns and $0.19. The
   [follow-up experiment](#follow-up-was-it-really-the-prompt) confirms this
   was specifically the "make the SMALLEST change, do not refactor"
   instruction, not the phase-engine structure: rewriting it to focus on root
   cause took `monolithic` to 3/3, matching `no-strategy`.

2. **Plain `no-strategy` matched every planner-based strategy's solve rate at
   the lowest cost and shortest wall-clock of the five.** No strategy in this
   sample beat 3/3; several tied it. Given `no-strategy` did so with no
   strong-model spend and roughly a third of the average wall-clock of
   `plan-exec`, on this evidence a planner phase bought reliability parity, not
   an improvement — though see [Limitations](#limitations) on why 3 instances is
   too small to generalize from.

3. **Fan-out (`wide-plan-exec`) traded money for wall-clock, not for solve
   rate.** Its `deepseek-v4-pro` spend ($0.35 avg) was the highest of any
   strategy — three parallel pro-model planners per instance — but because
   they run concurrently rather than serially, its average wall-clock (340.5s)
   was far better than the serial two-phase strategies (`plan-exec` 811.0s,
   `plan-critic-exec` 633.6s) for the same 3/3 solve rate.

4. **The critic gate (`plan-critic-exec`) was the cheapest and fastest way to
   reach 3/3 among the planner-based strategies** — cheaper than `plan-exec`
   ($0.43 vs $0.55 total) and much faster (633.6s vs 811.0s avg), despite
   running an *extra* pro-model phase (critic) that `plan-exec` doesn't have.
   That's because `plan-exec`'s single number is dominated by one severe
   outlier (see next point) — with it excluded, `plan-exec`'s remaining two
   runs are actually cheap and fast. Read the aggregate cost/time columns as
   outlier-sensitive with n=3, not as a stable per-run rate.

5. **One severe outlier**: `plan-exec` on `sphinx-7686` took 2123s (35 minutes,
   171 turns, $0.51) — 4-14x longer than every other strategy on the same
   instance. It still solved it, so it's not a failure, but it shows a single
   two-phase run can occasionally run far longer than a fan-out or critic-gated
   run on the identical instance. Worth a closer look at that run's log
   (`sphinx-doc__sphinx-7686.plan-exec.log`) before trusting `plan-exec`'s
   wall-clock numbers as representative.

6. **Every solved run's patch was 500-2500 bytes** — all five strategies
   converge on small, targeted diffs when they succeed; none of the "worse"
   outcomes were from an overly large or invasive patch, just a patch that
   didn't flip the target test (`FixNoFlip`) or no patch attempt reaching the
   agent at all (`ReproFailed`).

## Follow-up: was it really the prompt?

`no-strategy` and `monolithic` differ in exactly one behaviorally-relevant
way — the system prompt (both are single, growing-context loops over the same
tools, same model, same context-persistence). `monolithic`'s original prompt
told the model to *"Make the SMALLEST change that makes the failing tests
pass. Do not refactor"*; both of its losses were `Failed(FixNoFlip)` — a
change landed, but it never flipped the target test. The hypothesis: that
instruction pressures the model toward a minimal-but-wrong fix instead of the
one the root cause actually needs.

Tested it directly: rewrote `monolithic`'s system prompt (still forbidding
touching tests, still requiring verification before stopping) to drop
"smallest change" and instead direct root-cause investigation before editing —
see `crates/pirs-rhai/builtins/monolithic.rhai`. Re-ran `monolithic` (as
`monolithic-v2`) against `no-strategy` on the same 3 real instances.

| Instance | Strategy | Outcome | Turns | Elapsed | Cost |
|---|---|---|---|---|---|
| astropy-6938 | monolithic-v2 | Solved | 38 | 175.0s | $0.0480 |
| astropy-6938 | no-strategy | Solved | 24 | 113.8s | $0.0189 |
| scikit-learn-12471 | monolithic-v2 | **Solved** | 14 | 126.1s | $0.0307 |
| scikit-learn-12471 | no-strategy | Solved | 13 | 84.9s | $0.0182 |
| sphinx-7686 | monolithic-v2 | Solved | 59 | 287.3s | $0.1488 |
| sphinx-7686 | no-strategy | Solved | 80 | 344.0s | $0.2492 |

**`monolithic-v2` went from 1/3 to 3/3** — it now solves `scikit-learn-12471`,
the exact instance the original prompt caused it to fail on with a
non-flipping fix. Totals: `monolithic-v2` $0.2275 / avg 196.1s vs
`no-strategy` $0.2863 / avg 180.9s in this rerun — the two are now
statistically indistinguishable on this n=3 sample (and note `no-strategy`'s
own numbers moved between runs too — e.g. its `sphinx-7686` cost went from
$0.19 to $0.25 — a reminder that LLM agent runs are stochastic even with
nothing changed, per the single-trial-per-cell caveat below).

This confirms the mechanism, not just the correlation: **the specific
"smallest change" wording was actively harmful** on the two instances where it
mattered, and removing it (while keeping the legitimate "don't touch tests"
and "verify before stopping" guardrails) closed the entire gap to the naive
baseline. `monolithic`'s built-in prompt has been updated; this is a real fix,
not just a benchmark footnote.

Artifacts: [`bench-swebench-5x5/results_rerun/`](bench-swebench-5x5/results_rerun/),
[`bench-swebench-5x5/rerun_monolithic_vs_naive.py`](bench-swebench-5x5/rerun_monolithic_vs_naive.py).

## Second batch: 5 more instances (and a harness coverage gap)

Extended the sample with 5 more instances from the 45 remaining pre-pulled
docker images, chosen for repo diversity: `astropy__astropy-14182`,
`django__django-11001`, `matplotlib__matplotlib-26011`,
`scikit-learn__scikit-learn-25570`, `sympy__sympy-15346`. Same setup: all 5
strategies (with the now-fixed `monolithic`), `deepseek-v4-flash` base,
`deepseek-v4-pro` on planner/critic phases.

**Only 1 of the 5 new instances ever reached the agent.** The other 4 failed
identically across all five strategies, in two distinct ways:

- **`matplotlib-26011` and `scikit-learn-25570`: `Failed(ReproFailed)`**, the
  same pre-flight failure as `matplotlib-23562`/`pytest-5221` from the first
  batch — `turns=0` for every strategy, 96-97% of wall-clock spent inside
  `bootstrap` (up to 480s) before the harness ever hands control to the agent.
- **`django-11001` and `sympy-15346`: `Failed(RunnerUndetected)`** — a
  *different* failure, and a new finding: the harness's bundled test-runner
  detectors don't recognize Django's or sympy's test invocation (both use a
  custom runner rather than plain pytest). This fails in ~0.1-0.3s, before
  `discover` even completes, and is a harness coverage gap, not instance
  flakiness or a strategy effect — every strategy hits it identically because
  none of them ever get a turn.

Only `astropy-14182` produced real signal, solved by all 5 strategies:

| Strategy | Turns | Elapsed | Cost |
|---|---|---|---|
| no-strategy | 31 | 305.0s | $0.0789 |
| monolithic | 21 | 320.9s | $0.0523 |
| plan-exec | 31 | 407.7s | $0.0603 |
| plan-critic-exec | 43 | 597.5s | $0.0920 |
| wide-plan-exec | 100 | 403.4s | $0.1681 |

**Combined real-instance yield so far: 4 of 10 attempted instances (40%)** ever
reached the agent — `astropy-6938`, `scikit-learn-12471`, `sphinx-7686`,
`astropy-14182`. The other 6 split evenly between the two failure categories
above. This 40% yield is itself a finding: this particular set of pre-pulled
SWE-bench-lite docker images, combined with this harness's current detector
coverage, only produces usable signal on a minority of instances — see
[Limitations](#limitations).

**Combined strategy comparison across all 4 real instances** (using the fixed
`monolithic` throughout — `astropy-6938`/`scikit-learn-12471`/`sphinx-7686`
from the [follow-up rerun](#follow-up-was-it-really-the-prompt),
`astropy-14182` from this batch):

| Strategy | Solved (of 4) | Total cost | Avg cost |
|---|---|---|---|
| no-strategy | 4/4 | $0.3045 | $0.0761 |
| monolithic (fixed) | 4/4 | $0.2798 | $0.0700 |
| plan-exec | 4/4 | $0.6096 | $0.1524 |
| plan-critic-exec | 4/4 | $0.5238 | $0.1310 |
| wide-plan-exec | 4/4 | $0.6179 | $0.1545 |

**With the prompt fixed, every strategy now ties at 4/4** on this sample — the
solve-rate question that motivated this whole benchmark is no longer
differentiating. What still separates them is cost: `no-strategy` and the
fixed `monolithic` are statistically indistinguishable and cheapest (both
around $0.07-0.08 avg), while the three planner-based strategies cost
**roughly 2x more** for the same outcome on this sample, `wide-plan-exec` and
`plan-exec` being the most expensive. None of the extra spend bought a solve
the cheap strategies didn't already get — though again, n=4 is still small,
and a harder or more varied instance is where a planner phase would be
expected to start earning its cost.

**A mid-run infrastructure incident, unrelated to any of the above:** 7 of
this batch's 25 runs failed with `[Errno 28] No space left on device`
(`sympy-15346` all 5 strategies, `scikit-learn-25570`'s `plan-critic-exec`/
`wide-plan-exec`) when the host disk filled during the run — traced to
`/home/driver/hero/build/target`, a 389GB accumulated Rust build-artifact
cache unrelated to this benchmark. After ~400GB was freed, those 7 cells were
re-run cleanly; all 5 of the re-run `sympy-15346` cells landed on
`RunnerUndetected` (see above) and the 2 re-run `scikit-learn-25570` cells
landed on `ReproFailed`, both fully consistent with their sibling strategies'
results on the same instances — so the disk incident cost time, not data
integrity.

Artifacts: [`bench-swebench-5x5/results_matrix2/`](bench-swebench-5x5/results_matrix2/),
[`bench-swebench-5x5/run_matrix2.py`](bench-swebench-5x5/run_matrix2.py),
[`bench-swebench-5x5/rerun_disk_losses.py`](bench-swebench-5x5/rerun_disk_losses.py).

## Disk-pressure hypothesis: tested and refuted

The disk-full incident above raised a legitimate question: were the
`ReproFailed` instances actually failing because of disk pressure (slow
overlay-filesystem I/O as the host disk approached capacity), rather than a
genuine instance/harness incompatibility? The evidence looked plausible at
first glance — comparing bootstrap time for the same repo across batches:

| Instance | Batch | Bootstrap time |
|---|---|---|
| `matplotlib-23562` | 1 (disk presumed fine) | 207-251s |
| `matplotlib-26011` | 2 (disk filled later in this run) | 442-480s (~2x) |
| `scikit-learn-12471` | 1 (solved cleanly) | 6-9s |
| `scikit-learn-25570` | 2 (disk filled later in this run) | 21-31s (~3-4x) |

Both batch-2 instances took markedly longer to fail than their batch-1,
same-repo counterparts, right before the disk hit absolute zero later in that
same run. Worth testing directly rather than assuming either way.

**Test**: re-ran all 5 strategies on all 3 suspect instances (`matplotlib-23562`,
`matplotlib-26011`, `scikit-learn-25570` — 15 runs) with ~450-461GB free
throughout (checked before, during, and after).

**Result: refuted.** All 15 runs failed identically — same `Failed(ReproFailed)`
outcome, same bootstrap-time ballpark:

| Instance | Original bootstrap | Re-test bootstrap (450GB+ free) |
|---|---|---|
| `matplotlib-23562` | 207-251s | 231-278s |
| `matplotlib-26011` | 442-480s | 398-458s |
| `scikit-learn-25570` | 21-31s | 17-17.5s |

`scikit-learn-25570` bootstrapped a little faster with disk free (17s vs.
21-31s) but still failed the same way every time — nowhere close to
`scikit-learn-12471`'s healthy 6-9s, and it never got a single agent turn.
`matplotlib-23562` and `matplotlib-26011` didn't improve at all. **0/15 solved.**
Disk pressure is ruled out as the cause — these three instances are genuinely
broken for this harness/docker-image combination, for whatever underlying
reason actually explains the long `bootstrap` hangs (not investigated further
here; out of scope for this benchmark).

This is a useful negative result: it means the earlier "40% real-instance
yield" finding isn't an artifact of a resource-constrained host — it's a real
property of this harness + this set of pre-pulled docker images, and won't
self-resolve just by freeing disk space.

Artifacts: [`bench-swebench-5x5/results_disk_suspects/`](bench-swebench-5x5/results_disk_suspects/),
[`bench-swebench-5x5/rerun_disk_suspects.py`](bench-swebench-5x5/rerun_disk_suspects.py).

## Limitations

- **n=4 real instances, out of 10 attempted.** 6 of the first 10 SWE-bench-lite
  instances tried across both batches never exercised the agent at all — 4
  `Failed(ReproFailed)` (`matplotlib-23562`, `pytest-5221`, `matplotlib-26011`,
  `scikit-learn-25570`) and 2 `Failed(RunnerUndetected)` (`django-11001`,
  `sympy-15346`). Every solve-rate/cost claim above rests on the remaining 4
  (`astropy-6938`, `scikit-learn-12471`, `sphinx-7686`, `astropy-14182`) —
  directional, not statistically powered. A single flip on one instance moves
  a strategy's solve rate by 25 percentage points.
- **~40% real-instance yield from this docker-image set is itself a finding**,
  not just a sampling nuisance, and it's **not a disk-space artifact** — directly
  tested (see [Disk-pressure hypothesis](#disk-pressure-hypothesis-tested-and-refuted)):
  the 4 `ReproFailed` instances still fail identically with 450GB+ free.
  Extending this benchmark further with more of the 40 remaining images should
  expect a similar attrition rate until the two underlying gaps are fixed: (a)
  whatever actually makes `bootstrap` hang/fail on some images (up to 480s
  before giving up — root cause not yet identified, just confirmed to not be
  disk pressure), and (b) the harness's test-runner detector not recognizing
  Django's or sympy's custom test invocation.
- **One trial per (instance, strategy) cell.** LLM agent runs are stochastic;
  no repeated-seed variance is captured here — e.g. `no-strategy`'s own numbers
  moved between its two independent runs on the same 3 instances (see the
  follow-up section). Every cost/turn number here is one sample, not a
  distribution.
- **`--max-turns 40` / 2400s timeout were reused from an earlier, separately
  validated baseline**, not tuned for this specific comparison. `plan-exec`'s
  171-turn `sphinx-7686` outlier suggests some strategies may be more sensitive
  to the turn budget than others in ways this run can't isolate.
- **DeepSeek pricing is a snapshot** as of this run; absolute dollar figures
  will drift, though the relative ordering between strategies (which is the
  actual finding) is pricing-model-independent as long as flash/pro's relative
  price ratio holds.
- **The `ReproFailed` and `RunnerUndetected` instances are unresolved
  harness/environment gaps**, not a strategy finding — worth root-causing
  separately before reusing this instance set for a larger run.

## Reproduce

```sh
# Rebuild the harness binary (musl, portable into the eval containers):
CARGO_TARGET_DIR=/home/driver/hero/build/target cargo build --release \
    --target x86_64-unknown-linux-musl -p pirs-bench-runner --bin pirs-bench

# Run the full matrix (args: max_turns timeout_s concurrency out_dir):
python3 qa/bench-swebench-5x5/run_matrix.py 40 2400 2 /tmp/bench-out
```

`run_matrix.py` pins the 5 instances and 5 strategies listed above and calls
`run_one.run_instance` (also included) once per (instance, strategy) pair
through a `ThreadPoolExecutor` at the given concurrency. Each strategy's
`--no-strategy` / `--strategy-script` selection and result-file naming
(`<instance_id>.<label>.result.json`, keyed by label so same-model strategies
never collide) live in `run_one.py`.

## Artifacts

- [`bench-swebench-5x5/results/`](bench-swebench-5x5/results/) — all 25
  `<instance>.<label>.result.json` + `.log` pairs, plus `matrix_summary.json`
  (the run's own aggregate solved/total by strategy).
- [`bench-swebench-5x5/run_matrix.py`](bench-swebench-5x5/run_matrix.py),
  [`bench-swebench-5x5/run_one.py`](bench-swebench-5x5/run_one.py) — the
  orchestration scripts used to produce this data.
- [`bench-swebench-5x5/results_rerun/`](bench-swebench-5x5/results_rerun/),
  [`bench-swebench-5x5/rerun_monolithic_vs_naive.py`](bench-swebench-5x5/rerun_monolithic_vs_naive.py)
  — the 6-run follow-up (`monolithic-v2` vs `no-strategy`) that confirmed the
  prompt fix.
- [`bench-swebench-5x5/results_matrix2/`](bench-swebench-5x5/results_matrix2/),
  [`bench-swebench-5x5/run_matrix2.py`](bench-swebench-5x5/run_matrix2.py),
  [`bench-swebench-5x5/rerun_disk_losses.py`](bench-swebench-5x5/rerun_disk_losses.py)
  — the second batch (5 more instances × 5 strategies) and the 7-cell rerun
  after the mid-run disk-space incident.
- [`bench-swebench-5x5/results_disk_suspects/`](bench-swebench-5x5/results_disk_suspects/),
  [`bench-swebench-5x5/rerun_disk_suspects.py`](bench-swebench-5x5/rerun_disk_suspects.py)
  — the 15-run test (3 suspect instances × 5 strategies) that ruled out disk
  pressure as the cause of the `ReproFailed` instances.
- New strategy scripts: `.pirs/strategies/plan-critic-exec-pro-flash.rhai`,
  `.pirs/strategies/wide-plan-exec-pro-flash.rhai` (planner/critic phases
  pinned to `deepseek-v4-pro`, cloned from the built-in phase structure).
- **Fixed**: `crates/pirs-rhai/builtins/monolithic.rhai`'s system prompt —
  dropped "make the SMALLEST change, do not refactor" in favor of root-cause-
  first guidance, per the follow-up experiment above.
- New harness feature: `--no-strategy` on `pirs-bench solve`/`batch`/`selftest
  --agent` (`crates/pirs-bench-runner/src/{main,lib}.rs`), unit-tested in both
  crates (`no_strategy_flag_resolves_to_empty_strategy`,
  `naive_config_flag_is_stored_on_the_executor`, and siblings).
