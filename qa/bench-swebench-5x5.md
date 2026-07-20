# Strategy benchmark — 5 SWE-bench-lite instances × 5 execution modes

A live, real-API comparative study of pirs's execution strategies against real
SWE-bench-lite tasks, run inside the official `swebench/sweb.eval.*` docker
images. Every number below is pulled directly from a captured `.result.json` /
`.log` pair in `bench-swebench-5x5/results/`; nothing here is estimated.

**Reading order note:** this document is written in the order things were
actually discovered, including a wrong turn. The first ~150 lines describe
results from a harness that had a real bug (see
[Harness fix: unresolvable test ids no longer sink the baseline](#harness-fix-unresolvable-test-ids-no-longer-sink-the-baseline)),
which made 4 of the first 10 instances misreport as unusable
(`Failed(ReproFailed)`). That bug is now fixed, and **all 4 turned out to be
solvable, real instances** once it was — turning a 40%-yield, n=4 comparison
into an 80%-yield, n=8 one. **If you want the current, complete picture, skip
to the [combined 8-instance results](#combined-8-instance-results-current)
after the fix section** — the sections before it are preserved for the
investigative trail, not as the final numbers. The remaining 2 of 10
instances (`django-11001`, `sympy-15346`) needed a separate, larger fix —
see [Default fallback: agent-discovered test runners](#default-fallback-agent-discovered-test-runners)
— which mechanically works but exposed a real limit of its own, honestly
reported there rather than glossed over.

## Harness fix: unresolvable test ids no longer sink the baseline

**tl;dr: the "40% real-instance yield" and "2 unexplained ReproFailed
instances" findings below were a harness bug, not a property of these SWE-
bench-lite instances. Fixed in `crates/pirs-bench/src/command.rs`
(commit `92caaaa`); all 4 previously-"broken" instances now solve at
essentially the same rate as everything else.**

Investigating *why* `matplotlib-23562`, `matplotlib-26011`,
`scikit-learn-25570`, and `pytest-5221` all failed with `Failed(ReproFailed)`
(after first ruling out disk pressure — see
[below](#disk-pressure-hypothesis-tested-and-refuted)) led to the real cause:
this cached copy of the SWE-bench-lite dataset has **malformed `PASS_TO_PASS`
test ids** — parametrized pytest node ids truncated mid-comma during however
the dataset file was built, e.g.
`test_margins_errors[ValueError-args1-kwargs1-margin` (missing the closing
`, must be...]`). `pirs-bench`'s baseline capture runs the *entire* scope
(targets + all `PASS_TO_PASS` ids) in one combined `pytest` invocation. A
single unresolvable node id makes pytest exit with a usage error and report
**zero** test cases — not just for that one id, for the whole invocation,
including the real targets. With no case for the targets, the harness
concludes they never reproduced as red and reports `Failed(ReproFailed)` —
even though the targets themselves were perfectly fine.

Verified by hand before writing the fix: ran the exact combined command
`pirs-bench` uses inside a fresh `matplotlib-23562` container — confirmed
"no tests ran" (exit 4). Stripped just the 10 malformed ids out of 138 and
re-ran the identical command: it worked, 126 passed, 2 failed (exactly the
real, correctly-red targets), in 31 seconds.

**Fix:** `CommandRunner::run` (`crates/pirs-bench/src/command.rs`) now detects
this failure mode — zero JUnit cases from a non-empty batch — and parses
pytest's own `ERROR: not found: <path>` lines to identify exactly which ids
it couldn't resolve. Those ids are dropped and the batch is retried once; the
dropped ids still correctly report as `NotCollected` (they never resolved to
a real test — that's honest), but every id that *does* resolve gets its real
outcome instead of being swept into a false "not reproduced." Covered by a new
unit test, `one_unresolvable_id_no_longer_sinks_the_whole_batch`, with a fake
runner reproducing the exact all-or-nothing failure shape.

**Re-tested all 4 instances end-to-end** across all 5 strategies (20 runs)
with the fix live: **18/20 solved.** Both `matplotlib` instances now solve
5/5 across every strategy. The 2 remaining misses
(`scikit-learn-25570 [no-strategy]`, `pytest-5221 [no-strategy]`) are genuine
`Failed(FixNoFlip)` outcomes — real agent turns, real cost, a real (if
incomplete) fix attempt — not a harness artifact. These 4 instances are now
folded into the combined results below as first-class data, not exceptions.

Artifacts: [`bench-swebench-5x5/results_fixed_command/`](bench-swebench-5x5/results_fixed_command/),
[`bench-swebench-5x5/rerun_fixed_command.py`](bench-swebench-5x5/rerun_fixed_command.py).

## Combined 8-instance results (current)

All 8 real instances found across both batches, now that the harness bug is
fixed — `astropy-6938`, `scikit-learn-12471`, `sphinx-7686`, `astropy-14182`
(from the original runs) plus `matplotlib-23562`, `matplotlib-26011`,
`scikit-learn-25570`, `pytest-5221` (newly unlocked by the fix above) — all
using the corrected `monolithic` prompt:

| Strategy | Solved (of 8) | Total cost | Avg cost | Avg wall-clock |
|---|---|---|---|---|
| monolithic | **8/8** | $0.4503 | **$0.0563** | 323.0s |
| plan-critic-exec | 8/8 | $0.7325 | $0.0916 | 563.3s |
| plan-exec | 8/8 | $0.7572 | $0.0946 | 566.8s |
| wide-plan-exec | 8/8 | $1.0393 | $0.1299 | 422.5s |
| no-strategy | 6/8 | $0.5349 | $0.0669 | 285.2s |

**This flips the earlier picture.** With 4 more real instances added — 2 of
which (`pytest-5221`, `scikit-learn-25570`) turned out to be the hardest in
the set for the naive baseline — `no-strategy` is now the only strategy that
*doesn't* solve everything (6/8), while the fixed `monolithic` prompt solves
8/8 at the lowest cost of any strategy. The three planner-based strategies
also solve 8/8, but cost 1.6-2.3x more than `monolithic` for the same outcome.
`no-strategy`'s two misses were exactly the two instances where a bare, no-
system-guidance loop apparently wasn't enough — `monolithic`'s corrected
system prompt (still no strong model, same single growing-context loop)
picked up the slack that a fully generic prompt didn't.

The honest interpretation: **`monolithic`'s original prompt bug was real and
worth fixing** (the earlier 1/3-solved finding stands as history), but once
fixed, `monolithic` isn't just "as good as" `no-strategy` — on this 8-instance
sample it's better, and the planner-based strategies' extra cost still hasn't
bought a higher solve rate than the free system-prompt fix did. n=8 is still
a small sample (see [Limitations](#limitations)), so treat the *ranking* as
suggestive rather than final, but the *direction* — a well-written single-loop
prompt beating both a generic loop and three multi-phase planners on cost — is
a clear, reproducible result from this benchmark.

## Default fallback: agent-discovered test runners

The remaining 2 of the original 10 instances, `django-11001` and
`sympy-15346`, never made it into the 8-instance comparison above: both fail
`Failed(RunnerUndetected)` because their test suites use custom, non-pytest
invocations this harness's static rhai detectors don't recognize (confirmed
concretely for Django: its own docs say to run `tests/runtests.py`, a script
with no `pytest` involved at all, and `python -m pytest` isn't even
installed in its environment).

**A deeper wrinkle, not just "add a Django detector":** Django's own test
runner has **no JUnit XML output whatsoever**, and this harness's entire
architecture is built around JUnit as its one universal interchange format
— deliberately, to avoid a fragile per-framework text scraper. Checking the
pre-built environment confirmed no JUnit-bridging package
(`unittest-xml-reporting`, `pytest-django`) is installed either. A real fix
needs either installing such a bridge at bootstrap time, or a bespoke
Django-native result parser — a second, harder detection layer beyond just
"find the right shell command."

**What was built instead: a default (not opt-in) last-resort fallback.**
When no static detector confirms *any* runner, the harness now hands the
specific test ids to a bounded, edit-free sub-agent — read/bash/grep/find/ls
only, no `edit`/`write`/`ast_edit` — that investigates the repo (docs, CI
config, trial commands) and self-reports pass/fail via a forced
`report_test_results` tool call. If static detection succeeds normally (the
common case), this fallback is never invoked and costs nothing.

**This is a deliberate, explicit trust exception**, the only one in the
harness: every other `TestRunner` gets its verdict from independently
parsing a real subprocess's JUnit output; this one trusts the discovery
agent's own report instead. `InstanceReport.used_undetected_fallback` and the
JSONL trace's `self_reported_runner` field ensure this is never silently
indistinguishable from a harness-confirmed outcome.

### A real bug found and fixed by the first live test

The first live run against `django-11001` fell back correctly but came back
`Failed(BaselineUnusable)` after 162s with zero visible turns. Root cause:
the harness's baseline-stability check calls `TestRunner::run()` **twice**,
requiring the *same* answer both times, to guard against flaky tests — but
the discovery runner was doing two fully independent, non-deterministic LLM
investigations from scratch every call, so even a perfectly-investigated,
unchanged tree could disagree with itself on wording or edge cases. Every
agent-discovered instance would have failed this way, unconditionally.

**Fix:** cache the self-report keyed on (a fingerprint of the working tree's
current git state, the exact requested ids). An unchanged tree returns the
identical cached answer — guaranteed stable across repeat calls; a real fix
attempt changes the fingerprint, forcing a fresh investigation, so a post-fix
verify pass correctly reflects the new state rather than a stale pre-fix
answer. Covered by a dedicated test proving both halves (a second call on an
unchanged tree never re-invokes the provider; an edited tree does).

### Second live test: the mechanism works, and its real limit shows up honestly

With the caching fix, baseline capture succeeded — `BaselineUnusable` is
gone. The discovery agent then did something genuinely substantial: it
figured out Django's actual test invocation on its own and ran **all 120**
requested ids (2 targets + 118 keep-green) using the correct Django-native
dotted id format (`test_name (module.TestClass)`), reporting a real outcome
for every single one — no omissions, no guessing. That is exactly the kind
of "read the docs, try it, verify by actually running it" investigation this
fallback was built for, and it worked.

But one of the two `FAIL_TO_PASS` targets came back wrong:

| Target | SWE-bench ground truth | Agent self-report |
|---|---|---|
| `test_order_by_multiline_sql (...)` | should fail at base commit | `fail` ✓ |
| `test_order_of_operations (...)` | should fail at base commit | `pass` ✗ |

Since the reproduce gate requires *every* target to show red before trusting
a baseline, this one incorrect entry among 120 was enough to abort with
`Failed(ReproFailed)` — never reaching the actual fix loop. This is not a
mechanism failure: detection, fallback, investigation, and id formatting all
worked correctly. It is the trust trade-off the code's own doc comments
already name, materializing live: a self-reported outcome can simply be
wrong, on one test in 120, with nothing in the loop to independently catch
it. No further attempt was made to force a lucky pass on a re-run — the
honest result is that this mechanism unlocks Django/sympy-style instances
mechanically but inherits whatever accuracy the underlying model has at
actually running and reading test results, which is not perfect.

Artifacts: [`bench-swebench-5x5/results_agent_discovery/`](bench-swebench-5x5/results_agent_discovery/)
(both live-test logs/results, the issue text, and the test patch for
`django-11001`).

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

**[Superseded — see the fix above.](#harness-fix-unresolvable-test-ids-no-longer-sink-the-baseline)**
Both instances below turned out to be a harness bug (malformed test ids
poisoning the whole baseline run), not genuinely broken. Preserved as-written
for the investigative trail.

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

**Partially superseded — see the [fix](#harness-fix-unresolvable-test-ids-no-longer-sink-the-baseline)
and [combined results](#combined-8-instance-results-current) above.** The
`matplotlib-26011`/`scikit-learn-25570` `Failed(ReproFailed)` outcomes below
were the same harness bug as the first batch's, since fixed and re-tested
(`matplotlib-26011` now solves 5/5, `scikit-learn-25570` 4/5). Only
`django-11001`/`sympy-15346`'s `Failed(RunnerUndetected)` remains a real,
still-open gap — see [Limitations](#limitations). Preserved as-written for
the investigative trail.

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

This section's conclusion stands — disk pressure was correctly ruled out —
but its remaining premise (that these instances were "genuinely broken for
this harness/image combination" for some unidentified reason) was itself
wrong. Chasing that unidentified reason is exactly what led to the
[harness fix](#harness-fix-unresolvable-test-ids-no-longer-sink-the-baseline)
above. Preserved as-written for the investigative trail.

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

- **n=8 real instances, out of 10 attempted** (current, post-fix — see
  [combined results](#combined-8-instance-results-current)). `django-11001`
  and `sympy-15346` remain unusable through the strategy comparison — not
  because detection can't be worked around at all (the
  [default fallback](#default-fallback-agent-discovered-test-runners) does
  unlock `django-11001`'s detection and investigation end-to-end) but because
  the fallback's self-reported baseline came back with one wrong entry among
  120, tripping the reproduce gate before any strategy ever got a fix
  attempt. Every solve-rate/cost claim rests on the 8 real instances —
  directional, not statistically powered. A single flip on one instance moves
  a strategy's solve rate by 12.5 percentage points.
- **The agent-discovery fallback trades detection coverage for verification
  strength.** It mechanically works (confirmed live: correct Django-native
  investigation, correct dotted test ids, all 120 requested ids answered),
  but unlike every other runner in this harness, nothing independently
  double-checks its self-report — so a single mistaken pass/fail judgment
  (observed live, 1 wrong out of 120) can silently sink an otherwise-solvable
  instance before the fix loop even starts. This is not a bug to chase
  further; it's the trust trade-off, made real.
- **The earlier "~40% real-instance yield" and "root cause not yet
  identified" framing (both below, in the pre-fix sections) were wrong** —
  the yield was a specific, fixed harness bug (see
  [Harness fix](#harness-fix-unresolvable-test-ids-no-longer-sink-the-baseline)),
  not an inherent property of this docker-image set. Real yield with the fix
  is 8/10 (80%); the only remaining gap is the narrower
  `RunnerUndetected` issue, which affects test frameworks this harness's
  detectors don't yet cover (Django's and sympy's custom runners), not a
  general instability.
- **One trial per (instance, strategy) cell.** LLM agent runs are stochastic;
  no repeated-seed variance is captured here — e.g. `no-strategy`'s own numbers
  moved between its two independent runs on the same 3 instances (see the
  follow-up section). Every cost/turn number here is one sample, not a
  distribution.
- **`--max-turns 40` / 2400s timeout were reused from an earlier, separately
  validated baseline**, not tuned for this specific comparison. `plan-exec`'s
  171-turn `sphinx-7686` outlier (and its 2123s `sphinx-7686` run more
  generally) suggests some strategies may be more sensitive to the turn budget
  than others in ways this run can't isolate.
- **DeepSeek pricing is a snapshot** as of this run; absolute dollar figures
  will drift, though the relative ordering between strategies (which is the
  actual finding) is pricing-model-independent as long as flash/pro's relative
  price ratio holds.
- **The `RunnerUndetected` instances are a real, still-open harness gap** —
  worth fixing (add Django/sympy test-runner detectors) before reusing this
  instance set for a larger run, so all 10 (or more) instances contribute.

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
- [`bench-swebench-5x5/results_fixed_command/`](bench-swebench-5x5/results_fixed_command/),
  [`bench-swebench-5x5/rerun_fixed_command.py`](bench-swebench-5x5/rerun_fixed_command.py)
  — the 20-run re-test (4 previously-`ReproFailed` instances × 5 strategies,
  18/20 solved) confirming the harness fix below actually unlocked them.
- New strategy scripts: `.pirs/strategies/plan-critic-exec-pro-flash.rhai`,
  `.pirs/strategies/wide-plan-exec-pro-flash.rhai` (planner/critic phases
  pinned to `deepseek-v4-pro`, cloned from the built-in phase structure).
- **Fixed**: `crates/pirs-rhai/builtins/monolithic.rhai`'s system prompt —
  dropped "make the SMALLEST change, do not refactor" in favor of root-cause-
  first guidance, per the follow-up experiment above.
- **Fixed**: `crates/pirs-bench/src/command.rs`'s `CommandRunner::run` — a
  single unresolvable test id no longer sinks an entire baseline batch to
  zero cases; the harness now drops exactly the unresolvable ids (identified
  from pytest's own stderr) and retries, per the
  [harness fix](#harness-fix-unresolvable-test-ids-no-longer-sink-the-baseline)
  above. New regression test:
  `one_unresolvable_id_no_longer_sinks_the_whole_batch`.
- New harness feature: `--no-strategy` on `pirs-bench solve`/`batch`/`selftest
  --agent` (`crates/pirs-bench-runner/src/{main,lib}.rs`), unit-tested in both
  crates (`no_strategy_flag_resolves_to_empty_strategy`,
  `naive_config_flag_is_stored_on_the_executor`, and siblings).
- New default harness feature: the agent-discovered test-runner fallback
  (`crates/pirs-bench-runner/src/agent_runner.rs`, `crates/pirs-bench/src/harness.rs`'s
  new `undetected_fallback` parameter on `run_instance`) — see
  [Default fallback: agent-discovered test runners](#default-fallback-agent-discovered-test-runners).
  Unit-tested: `harness.rs`'s
  `undetected_fallback_bypasses_bootstrap_and_is_flagged_in_the_report` (wiring,
  no LLM call), `agent_runner.rs`'s
  `self_reported_outcomes_land_in_the_snapshot_and_omitted_ids_are_not_collected`
  and `same_tree_state_reuses_the_cached_self_report_without_a_second_investigation`
  (the tool-dispatch path and the tree-fingerprint caching fix, both against a
  scripted provider). Live-tested twice against `django-11001`; artifacts in
  [`bench-swebench-5x5/results_agent_discovery/`](bench-swebench-5x5/results_agent_discovery/).
