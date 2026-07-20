# QA — feature verification with proof

Every claim below is backed by a captured artifact in this directory. Live runs
were executed against DeepSeek (`--provider openai --base-url
https://api.deepseek.com`) using `deepseek-v4-flash` / `deepseek-v4-pro`. All
logs were scrubbed of API keys before saving (`--api-key <redacted>` in place of
the real secret); no keys or secrets are committed.

## Static gates

| Gate | Proof | Result |
|------|-------|--------|
| Full test suite | `test-suite.txt` | **507 passed, 0 failed** |
| Formatting | `fmt.txt` | clean (`cargo fmt --check`, exit 0) |
| Lint | `clippy.txt` | clean (`clippy -D warnings`, exit 0) |
| CLI surface | `cli-help.txt` | `--help` renders all flags |

Reproduce:

```sh
cargo test --workspace            # -> test-suite.txt
cargo fmt --all --check           # -> fmt.txt
cargo clippy --workspace --all-targets -- -D warnings   # -> clippy.txt
```

## Live feature runs

Each log is a full transcript of the product agent (`pirs`) exercising one
feature end to end against a real model.

| # | Feature | Proof | What it demonstrates |
|---|---------|-------|----------------------|
| 1 | Naive coding loop (pi mode) | `live/01-naive-loop.log` | The agent runs a task with no strategy — plain tool-use loop to completion. |
| 2 | `--strategy plan-exec` | `live/02-strategy-plan-exec.log` | Read-only plan phase → full-scope exec phase, `{prev}` threaded between them. |
| 3 | Oracle routing (`plan-oracle-exec`) | `live/03-oracle-routing.log` | Per-phase model override: only the critic phase runs on the stronger model. |
| 4 | Wide fan-out (`general-wide-plan-exec`) | `live/04-wide-fanout.log` | Three parallel read-only planners merge under `## Branch N` for one executor. |
| 5 | `--profile security-reviewer` | `live/05-profile-security-reviewer.log` | Provider-agnostic profile drives a review pass (no hard-pinned model). |
| 6 | Verify-and-retry gate (pass) | `live/06-gate-pass.log` | `--verify` command runs; passing verify completes the run cleanly. |
| 7 | Verify gate (fail → exit 1) | `live/07-gate-fail-exit1.log` | Exhausted attempts with a failing `--verify` exits non-zero (real exit 1). |

## Orchestration & runtime control (live)

Live runs proving the agent-loop control surface and multi-instance features.

| # | Feature | Proof | What it demonstrates |
|---|---------|-------|----------------------|
| 8 | Background jobs + waiter + monitoring | `live/08-background-jobs.log` | `bash(background:true)` → job #1; `wait_ready` confirms the server is listening; `jobs`/`job_output` monitor it; `job_kill` stops it (`running`→`killed`). |
| 9 | Steering a running turn | `live/09-steering-rpc.log` | Over `--mode rpc`, a `steer` message injected mid-turn (while a tool was executing) lands as a user message inside the running conversation and redirects the model — it abandons its 4-step plan and answers the steered question instead. |
| 10 | Fleets (orchestrator) | `live/10-fleet-orchestrator.log` | `pirs-orchestrator` daemon + `spawn` of two headless workers; `list` shows both `online`; per-instance `rpc` prompts run independently; **isolation** proven (worker-a writes `ALPHA` in its cwd, worker-b writes `BETA` in its); `status`; `stop` → `no instances`. |
| 11 | Swarm / hive coordination | `live/11-swarm-hive.log` | Two separate `pirs` processes coordinate over the shared `swarm.jsonl` blackboard (`swarm.rhai` pack): a queen `swarm_post`s two packets, a worker `swarm_claim`s + `swarm_done`s one; final blackboard shows `#1 done`, `#2 open`. Also exercises the extension re-entrancy guard. |

These map to: background-job tools (`jobs`/`job_output`/`job_kill`/`job_wait`/`wait_ready`/`job_steer`),
`agent.steer()` via the RPC `steer`/`prompt` commands, the `pirs-orchestrator`
Unix-socket fleet control (`spawn`/`list`/`status`/`stop`/`rpc`), and the
Rhai swarm pack over a shared JSONL queue.

## Incremental graph index (`--persist-graph`)

Persistent, incrementally-refreshed code-graph cache (SQLite). Skips re-parsing
unchanged files on warm starts — the scaling lever for large repos.

| # | Feature | Proof | What it demonstrates |
|---|---------|-------|----------------------|
| 12 | Incremental graph store | `live/12-persist-graph.log` | On the pirs repo: **cold** run parses all 142 files → 2065 symbols, writes `.pirs/graph.db` (0.9s); **warm** run re-parses **0**, all 142 unchanged, same 2065 symbols (0.2s, ~4.5× faster); after touching one file, **exactly 1** re-parsed, 141 unchanged — same 2065 symbols. Symbol count identical across all three = equivalence holds live. |

Correctness is also test-pinned in `crates/pirs-graph/tests/store_test.rs`:
`incremental_refresh_equals_full_parse_across_add_change_delete` asserts the
incrementally-refreshed graph is set-equivalent to a from-scratch parse across
adds, changes, and deletes; `corrupt_db_is_recreated_not_fatal` proves a garbage
cache is wiped and rebuilt rather than breaking the agent.

## Hybrid code search (`code_search`)

One tool fuses three complementary retrieval signals with reciprocal-rank
fusion (RRF, k=60): **BM25 lexical** (tantivy, in-RAM), **embedding cosine**
(optional, OpenAI-compatible service, no native ONNX dep), and **graph
centrality** (caller count). BM25 needs no model and builds instantly, so the
tool works the moment the graph exists; it registers whenever the graph is on.
The embedding arm activates under `--semantic` + `--embed-model`. By default a
**background indexer** fills the embedding index off the search path (BM25
answers instantly; semantic hits light up as vectors land), checkpointing every
batch so a restart resumes instead of re-embedding. `--embed-batch-cap N` opts
into synchronous inline indexing for a one-shot. Either way it degrades silently
to lexical+graph if the service is down or the index is empty.

| # | Feature | Proof | What it demonstrates |
|---|---------|-------|----------------------|
| 13 | `semantic_search` (embedding-only, superseded) | `live/13-semantic-search.log` | Historical: the original embedding-only tool on `all-minilm` returned plausible-but-wrong hits (`diff`/`not_found_error`) for a staleness query — the weakness that motivated the hybrid. |
| 14 | `code_search` hybrid (BM25+embeddings+graph) | `live/14-code-search-hybrid.log` | On the pirs repo, `all-minilm` embedded **all 2124 symbols**; the tool reports `[lexical+semantic+graph]`. **Same query as #13** now ranks the real incremental-refresh/staleness symbol #1. An exact-identifier query returns `store_embeddings`/`ensure_model`/model-guard test as ranks 1-3 — BM25's exact-term strength that cosine alone lacked. |
| 15 | Background indexer + nomic quality | `live/15-background-index-and-nomic.log` | An idle `repl` built the full **2137-symbol** `nomic-embed-text` index in the background (64→2137 in ~7 min) while staying responsive; count persisted across a kill. **Honest finding:** nomic-embed-text is *not* meaningfully better than all-minilm on these queries — both are general models; the steering query still misses `steer`. Confirms a code-specific embedder is the real quality lever. |
| 16 | Code-specific embedder proven the lever | `live/16-code-embedder-vs-general.log` | Isolating the semantic arm (pure cosine, no BM25, on queries with zero lexical overlap): three **general** models (all-minilm, nomic, OpenAI text-embedding-3-small) all miss `steer`; **`codestral-embed`** (code-trained) ranks `steer` + both steering tests top-6, and at 4000-char chunks puts the real `refresh` function #1. Earlier "all embedders look alike" was a BM25-domination artifact. Cost: ~$0.04 to index the whole repo. |

Correctness/robustness is also test-pinned:
- `crates/pirs-graph/tests/bg_index_test.rs` — the background indexer fills the
  whole index off the search path (each symbol embedded exactly once), and a
  kill mid-build resumes from the last checkpoint (run 2 embeds only the
  remainder, never a full re-index).
- `crates/pirs-ai/tests/embed_client_test.rs` — the embeddings client parses
  responses, realigns out-of-order indexes, rejects count mismatches, surfaces
  non-2xx as errors.
- `crates/pirs-graph/tests/lexical_test.rs` — BM25 ranks the exact-term owner
  first; punctuated/empty natural-language queries are safe (no tantivy grammar
  errors).
- `crates/pirs-graph/tests/store_test.rs::semantic_embed_store_search_and_model_guard`
  — embed/store/search ranking, the model-swap wipe guard, incremental re-embed
  on file change.
- `crates/pirs-graph/tests/code_search_test.rs` — **a bug live testing caught**:
  small-context models (all-minilm, 256 tokens) reject dense chunks; the fix
  truncates the offender per-item so one oversized symbol never aborts the index.
  A second test asserts the tool still returns BM25 results when the embedding
  service is dead (graceful lexical+graph fallback).

**Quality lever — settled (#16).** BM25 anchors exact terms so a weak embedder
never sinks a query, but the *semantic* arm's quality is entirely the model's.
Three **general** models (all-minilm, nomic-embed-text, OpenAI
text-embedding-3-small) all fail domain-concept queries — none maps "inject an
instruction mid-flight" to `steer`. A **code-trained** model, `codestral-embed`,
nails it (isolated-semantic proof in #16), and fuller chunks (`--embed-max-chars
4000`, within its 8K window) push the real target function to rank #1. So for
meaningful semantic code search, use a code embedder:

    --semantic --embed-base-url https://openrouter.ai/api/v1 \
    --embed-model mistralai/codestral-embed-2505 --embed-api-key <key> \
    --embed-max-chars 4000

It's a one-time ~$0.04 to index this repo; the background indexer and OpenAI-
compatible client already consume it. Not a wiring gap — a model choice. (Cloud
embeddings send code off-machine: fine for a public repo, weigh for private.)

## Strategy comparison benchmark (SWE-bench-lite)

A live, real-API comparison of all 5 execution modes (`no-strategy`,
`monolithic`, `plan-exec`, `plan-critic-exec`, `wide-plan-exec`) against 10
SWE-bench-lite instances (two batches of 5) inside the official eval docker
images — 50 runs attempted, ~$2.90 total spend. Full methodology, per-run
results, and findings in [`bench-swebench-5x5.md`](bench-swebench-5x5.md); raw
`.result.json`/`.log` artifacts in
[`bench-swebench-5x5/results/`](bench-swebench-5x5/results/) and
[`results_matrix2/`](bench-swebench-5x5/results_matrix2/).

Headline #1: `monolithic`'s original prompt ("make the SMALLEST change... do
not refactor") was dominated on every axis by the plain `no-strategy`
baseline — traced to that one instruction pressuring the model into
minimal-but-wrong fixes. A follow-up experiment rewrote the prompt to focus on
root cause instead and re-ran it: `monolithic` went from 1/3 to 3/3, closing
the entire gap. The built-in prompt
(`crates/pirs-rhai/builtins/monolithic.rhai`) has been fixed accordingly —
this was a real bug, not just a benchmark footnote.

Headline #2: only **4 of the 10 attempted instances ever reached the agent**
— the other 6 failed identically across all five strategies, either
`Failed(ReproFailed)` (a harness/environment pre-flight gap, 4 instances) or
`Failed(RunnerUndetected)` (the harness's test-runner detector doesn't
recognize Django's or sympy's custom test invocation, 2 instances). On the 4
real instances, every strategy now ties at 4/4 solved (with the fixed
`monolithic`) — the remaining differentiator is cost, where `no-strategy` and
`monolithic` are cheapest and the three planner-based strategies cost roughly
2x more for the same outcome.

## Discovery

| Feature | Proof | What it demonstrates |
|---------|-------|----------------------|
| Strategy name resolution | `strategy-discovery.txt` | Built-ins resolve by name; unknown names fail with a helpful error. |

## Notes

- Built-in strategies (`monolithic`, `plan-exec`, `plan-critic-exec`,
  `wide-plan-exec`) are embedded in `pirs-rhai`; project `.pirs/strategies/`
  overrides and additions (e.g. `plan-oracle-exec`, `general-*`) resolve by name
  with project-then-home precedence.
- `pirs-bench solve`/`batch`/`selftest --agent` also accept `--no-strategy`,
  which bypasses the strategy engine entirely for a true naive baseline (the
  interactive CLI's own default when no `--strategy`/`--profile` is given) —
  see the benchmark above for what that baseline actually costs/solves versus
  the built-ins.
- The extension packs are cataloged in `../extensions/README.md` and loaded by
  the `pirs-rhai` integration tests, counted in the 507 above.
