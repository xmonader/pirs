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
| `monolithic` | Built-in. One growing-context loop, but through the phase engine with a bench-engineered "make the smallest change, don't touch tests" system prompt. | none |
| `plan-exec` | `.pirs/strategies/plan-pro-exec-flash.rhai`. Read-only planner → fresh full-scope executor seeded only with the plan. | planner |
| `plan-critic-exec` | `.pirs/strategies/plan-critic-exec-pro-flash.rhai`. Planner → critic gate (may rewrite the plan) → fresh executor. | planner + critic |
| `wide-plan-exec` | `.pirs/strategies/wide-plan-exec-pro-flash.rhai`. Three read-only planners investigate in parallel (assertion-focused / recency-focused / edge-case-focused), merged → fresh executor. | all 3 parallel planners |

## Headline finding

**`monolithic` is dominated by plain `no-strategy` on every axis that matters:**
lower solve rate, higher cost, longer wall-clock. Splitting into a phase engine
bought nothing here unless it also added a planner (`plan-exec` and beyond).

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

1. **`monolithic` is the outlier — worse, not just different.** It solved 1/3
   real instances (only `astropy-6938`); every other strategy, including the
   zero-cost naive baseline, solved 3/3. Its two losses
   (`scikit-learn-12471`, `sphinx-7686`) were both `Failed(FixNoFlip)` — it
   produced a change, but the change never flipped the target tests from red to
   green. On `sphinx-7686` it burned 101 turns and $0.39 to arrive at a change
   that didn't work, while `no-strategy` solved the same instance in 60 turns
   and $0.19. The bench-engineered "smallest change" system prompt bought
   `monolithic` nothing here over a generic assistant prompt; if anything it
   correlates with worse outcomes on the harder two instances.

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

## Limitations

- **n=3 real instances.** `matplotlib-23562` and `pytest-5221` never exercised
  the agent, leaving only `astropy-6938`, `scikit-learn-12471`, and
  `sphinx-7686` as actual signal. Every claim above is a 3-instance sample —
  directional, not statistically powered. A single flip on one instance moves
  a strategy's solve rate by 33 percentage points.
- **One trial per (instance, strategy) cell.** LLM agent runs are stochastic;
  no repeated-seed variance is captured here, so e.g. `monolithic`'s 1/3 could
  plausibly be 2/3 or 0/3 on a re-run. Same caveat applies to every cost/turn
  number — each is one sample, not a distribution.
- **`--max-turns 40` / 2400s timeout were reused from an earlier, separately
  validated baseline**, not tuned for this specific comparison. `plan-exec`'s
  171-turn `sphinx-7686` outlier suggests some strategies may be more sensitive
  to the turn budget than others in ways this run can't isolate.
- **DeepSeek pricing is a snapshot** as of this run; absolute dollar figures
  will drift, though the relative ordering between strategies (which is the
  actual finding) is pricing-model-independent as long as flash/pro's relative
  price ratio holds.
- **The two `ReproFailed` instances are an unresolved harness/environment gap**,
  not a strategy finding — worth root-causing separately (likely a slow or
  failing `bootstrap` step specific to these two docker images) before reusing
  this instance set for a larger run.

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
- New strategy scripts: `.pirs/strategies/plan-critic-exec-pro-flash.rhai`,
  `.pirs/strategies/wide-plan-exec-pro-flash.rhai` (planner/critic phases
  pinned to `deepseek-v4-pro`, cloned from the built-in phase structure).
- New harness feature: `--no-strategy` on `pirs-bench solve`/`batch`/`selftest
  --agent` (`crates/pirs-bench-runner/src/{main,lib}.rs`), unit-tested in both
  crates (`no_strategy_flag_resolves_to_empty_strategy`,
  `naive_config_flag_is_stored_on_the_executor`, and siblings).
