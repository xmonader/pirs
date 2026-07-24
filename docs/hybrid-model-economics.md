# Strong plan, weak exec: what we measured with pirs

*Research note — July 2026*

Recent posts on [agent swarms and model economics](https://cursor.com/blog/agent-swarm-model-economics) argue that a strong model can own planning while cheaper models do most of the token-heavy work, keeping quality similar and cutting cost. We ran a series of controlled experiments with **pirs** to see how much of that story shows up in a practical coding-agent harness—not a multi-agent swarm, but the dual-model pattern pirs already ships:

```bash
pirs --model <cheap-executor> --plan-model <strong-planner> --strategy plan-exec "…"
```

This note summarizes the setup, results, and limits of what we can claim.

---

## Takeaways

1. **Dual-model plan/exec works end-to-end in pirs.** Planning can run on one model; execution on another, with separate token accounting.
2. **When hybrid succeeds, the token shape matches the economics story:** the planner uses a minority of tokens; the executor does most of the tool loop.
3. **Solve-rate is often flat across mixes**—not because hybrid is magic, but because **mid-tier models already solve the task alone** when they get full tools and a test oracle.
4. **The harness inflates “weak mono” success.** Ablations show edit tools and the ability to run tests are doing a large share of the work.
5. **When the weak model cannot use tools reliably, a strong plan alone does not rescue it.** Putting the strong model on **execution** can still succeed.
6. **These results do not evaluate multi-agent swarms** (shared VCS, hundreds of workers, multi-hour greenfield builds). They evaluate **thin dual-model strategies** on pytest-gated coding tasks.

---

## What we ran

### Hybrid matrix (four cells)

| Cell | Planner | Executor | Question |
|------|---------|----------|----------|
| **A** | — | Weak only | Can the cheap model solve alone? |
| **B** | — | Strong only | Can the strong model solve alone? |
| **C** | Strong | Weak | Cursor-style hybrid: quality and cost? |
| **D** | Weak | Strong | Does role assignment matter? |

**Strong model** in most runs: `kimi-for-coding`.  
**Weak models tried:** Qwen3 Coder Plus, MiniMax M2 / M2.5, Mercury 2, Gemma 3 12B, and others.  
**Success:** independent `pytest` after the agent run (and/or harness `--verify`).

### Tasks

| Task | Difficulty (for tool-using agents) | What it stresses |
|------|--------------------------------------|------------------|
| **Raft subset** | Easy–medium | Classic figure-2 rules (votes, log install, commit) |
| **MVCC / snapshot isolation** | Medium | Concurrent visibility, write–write conflicts, write-skew, phantoms |

Raft was useful for plumbing checks but too easy for most mid models with tools. MVCC forced more thrash and tokens while remaining unit-test scale.

We deliberately **did not** reimplement a multi-agent swarm with custom merge infrastructure. pirs `plan-exec` is a two-phase, single-agent workflow with an optional second model on the plan phase.

---

## Results

### When the weak model can tool-call

On Raft and MVCC, models such as **Qwen3 Coder Plus**, **MiniMax M2**, and **Mercury 2** typically:

- **Solved mono (cell A)**
- **Solved strong mono (cell B)**
- **Solved hybrid C and reverse D**

So there was often **no solve-rate gap** to close. Hybrid still mattered for **cost shape**.

**Example — MiniMax M2 as executor, Kimi as planner (Raft, hardened task)**

| Cell | Solved | Wall time | Tokens (approx.) |
|------|--------|-----------|------------------|
| A · M2 mono | yes | 51s | M2 ~40k |
| B · Kimi mono | yes | 34s | Kimi ~65k |
| **C · Kimi plan → M2 exec** | **yes** | **100s** | **Plan ~15k + exec ~23k** |
| D · M2 plan → Kimi exec | yes | 49s | Plan ~9k + exec ~96k |

Cell **C** is the economic pattern: planning is a small slice; execution burns most tokens on the cheaper model.

**Example — harder MVCC task, same pair**

| Cell | Solved | Wall time | Tokens (approx.) |
|------|--------|-----------|------------------|
| A · M2 mono | yes | **222s** (multiple verify attempts) | M2 **~255k** |
| B · Kimi mono | yes | **73s** | Kimi ~152k |
| **C · Kimi plan → M2 exec** | **yes** | 181s | **Plan ~22k + exec ~24k** |
| D · M2 plan → Kimi exec | yes | 530s | Plan ~9k + exec ~139k |

Here hybrid **C** preserved quality while using far fewer tokens than a thrashing weak mono. That is the practical “model economics” win on these tasks: **not higher pass rate, lower waste**.

**Qwen3 Coder Plus** (DashScope) with Kimi showed the same pattern on Raft: all cells green; C with a small plan bill and larger executor bill.

### When the weak model breaks tool use

**Gemma 3 12B** behaved differently:

| Cell | Result |
|------|--------|
| A · Gemma mono | **Fail** |
| B · Kimi mono | Pass |
| C · Kimi plan → Gemma exec | **Fail** |
| D · Gemma plan → Kimi exec | **Pass** |

Logs showed Gemma emitting **markdown/JSON pseudo-tool calls** instead of native tool calls—little or no real patch. A strong plan did not fix that. A strong **executor** did.

**Implication:** “Put the frontier model on planning” only helps if the cheap model can still **act**. If the failure mode is tool protocol, you want strong execution (or better weak-model tool adaptation—e.g. pirs `--weak`), not only a better plan.

### Liquid LFM

We could not run `liquid/lfm-*` (including `lfm-2-24b-a2b`) on the OpenRouter account available for these runs (no live endpoints). That is an availability limit, not a quality result.

---

## Are we just measuring the harness?

We ran a **tool ablation** on the harder MVCC task with MiniMax M2 only (mono), holding the prompt fixed:

| Condition | Solved? | What it means |
|-----------|---------|----------------|
| Full tools + harness `pytest` verify | **Yes** | Current product path |
| Full tools, no harness verify | **Yes** | Agent self-ran `pytest` via shell |
| Edit tools, **pytest blocked** | **No** (11/12 tests) | Partial fix without a clean oracle |
| Read-only tools | **No** | Cannot land a patch |
| Essentially no tools | **No** | No meaningful fix |

So for mid models on unit-test tasks:

- **Edit + a test oracle** (harness or self-run) drive most mono success.
- Removing the oracle hurts even when edits are allowed.
- “Weak mono solves” is partly **agent stack skill**, not pure model IQ.

That does not invalidate hybrid economics—it means **solve rate alone is a blunt metric**. Token and attempt counts under a fixed harness are more informative when everyone can pass.

### No-strategy (naive loop)

Omitting `--strategy` entirely runs an undivided one-shot agent loop (no multi-phase engine, no harness `--verify`). On the hard MVCC task:

| Cell | Mode | Model | Solved? | Time | Tokens (approx.) |
|------|------|-------|---------|------|------------------|
| NS weak | no strategy | MiniMax M2 | **No** | 10s | ~35k |
| Mono weak | monolithic + verify | MiniMax M2 | **Yes** | 153s | ~189k |
| NS strong | no strategy | Kimi | **Yes** | 351s | ~75k |
| Mono strong | monolithic + verify | Kimi | **Yes** | 153s | ~79k |

**Read:** For the mid model, **strategy + verify** turned a quick fail into a pass (at higher token cost). The strong model could still succeed without strategy, but was **slower** than mono+verify. Orchestration and feedback loops change outcomes as much as model tier.

> Hybrid cells **C/D require** a multi-phase strategy (`plan-exec`) so `--plan-model` can attach to the plan phase. “No strategy” is a mono-style baseline, not a hybrid mode.

---

## How this relates to public swarm results

Cursor’s write-up emphasizes:

- A mature **swarm harness** (task trees, coordination, review), and  
- **Similar quality across model mixes** with large **cost** differences on a huge greenfield job.

Our work emphasizes:

- A **single-agent** dual-model strategy in pirs, and  
- **Pytest-gated** coding tasks at small/medium scale.

| Theme | Alignment |
|-------|-----------|
| Planner tokens ≪ worker tokens | **Aligned** when C succeeds |
| Quality flat across mixes | **Aligned** for tool-capable mids; **not** for tool-broken weaks |
| Strong plan is the scarce resource | **Only sometimes**—sometimes strong **exec** matters more |
| Swarm coordination quality | **Out of scope** |

Treat this as a **product-relevant bound** on dual-model plan/exec in pirs, not a replication of multi-agent swarm benchmarks.

---

## Using this in pirs today

**Strong plan, weak exec:**

```bash
pirs --model <cheap> --plan-model <strong> --strategy plan-exec \
  --verify "python -m pytest -q" \
  "Fix the failing tests without editing them."
```

**Weak-model hardening** (tool diet, sequential tools, retries; pairs well with `--plan-model`):

```bash
pirs --weak --model <cheap> --plan-model <strong> --strategy plan-exec \
  --verify "python -m pytest -q" \
  "…"
```

**Notes from the experiments:**

- Prefer weak models that **reliably call tools** if you care about hybrid C.
- Use `--verify` (or ensure the agent can run tests) so execution is grounded.
- Judge hybrid success on **pass rate and tokens/latency**, not pass rate alone.
- For fair model comparisons, hold tools and verify policy fixed across A/B/C/D.

Docker SWE-bench-style runs can use the same idea when the bench binary supports `--plan-model` (same provider/base URL for both model ids). True dual-backend routing is available on the host CLI via the model registry (`backend/model` pins).

---

## Limitations

- Small number of tasks and seeds; not a large leaderboard.
- Tasks remain **unit-test microbenchmarks**, not multi-hour repository construction.
- Prices were not always metered in dollars—**token counts** are the cost proxy.
- Some providers/models were unavailable (e.g. Liquid LFM on the OpenRouter route we used).
- No multi-agent swarm, merge queue, or long-running shared codebase.

---

## What we’ll keep measuring

1. **Efficiency hybrids** — when A and B both pass, does C cut tokens and attempts?  
2. **Rescue hybrids** — when A fails and B passes, does C recover?  
3. **Tool ablations** — so we don’t mistake harness power for model tier.  
4. **Harder, less oracle-shaped tasks** — where pytest cannot spoon-feed the fix.

---

## Summary table

| Weak model | Task class | A | B | C (strong→weak) | D (weak→strong) | Main lesson |
|------------|------------|---|---|-----------------|-----------------|-------------|
| Qwen3 Coder Plus | Raft | pass | pass | pass | pass | Flat quality; C cheap on plan tokens |
| MiniMax M2 | Raft | pass | pass | pass | pass | Same |
| MiniMax M2 | MVCC | pass* | pass | pass | pass | *Thrash on A; C much leaner |
| Mercury 2 | Raft | pass | pass | pass | pass | Flat quality |
| Gemma 3 12B | Raft | fail | pass | fail | pass | Tool use blocks C; strong exec helps |
| Liquid LFM | — | — | — | — | — | Not available on our route |

---

## Closing

**Strong plan + weak exec is real and usable in pirs today.** It shows up most clearly as **cost and thrash reduction** when the weak model can already operate the agent loop. It is **not** a substitute for tool-capable executors, and it is **not** the same claim as full agent-swarm scaling.

If you run your own matrix, keep the four cells, fix the tool policy, and report tokens alongside pass/fail—that combination is what made these results interpretable.

---

*Experiments run with pirs against live model APIs (Kimi Coding Plan, DashScope, OpenRouter, and others). Pass/fail judged by project tests after each agent run.*
