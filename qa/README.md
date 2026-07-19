# QA — feature verification with proof

Every claim below is backed by a captured artifact in this directory. Live runs
were executed against DeepSeek (`--provider openai --base-url
https://api.deepseek.com`) using `deepseek-v4-flash` / `deepseek-v4-pro`. All
logs were scrubbed of API keys before saving (`--api-key <redacted>` in place of
the real secret); no keys or secrets are committed.

## Static gates

| Gate | Proof | Result |
|------|-------|--------|
| Full test suite | `test-suite.txt` | **505 passed, 0 failed** |
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
The embedding arm activates under `--semantic` + `--embed-model`, is bounded
(`--embed-batch-cap`, default 256 symbols/call, persisted), and degrades
silently to lexical+graph if the service is down or the index is empty.

| # | Feature | Proof | What it demonstrates |
|---|---------|-------|----------------------|
| 13 | `semantic_search` (embedding-only, superseded) | `live/13-semantic-search.log` | Historical: the original embedding-only tool on `all-minilm` returned plausible-but-wrong hits (`diff`/`not_found_error`) for a staleness query — the weakness that motivated the hybrid. |
| 14 | `code_search` hybrid (BM25+embeddings+graph) | `live/14-code-search-hybrid.log` | On the pirs repo, `all-minilm` embedded **all 2124 symbols**; the tool reports `[lexical+semantic+graph]`. **Same query as #13** now ranks the real incremental-refresh/staleness symbol #1. An exact-identifier query returns `store_embeddings`/`ensure_model`/model-guard test as ranks 1-3 — BM25's exact-term strength that cosine alone lacked. |

Correctness/robustness is also test-pinned:
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

**Honest quality note.** The hybrid closes most of the gap the embedding-only
tool had on `all-minilm`: BM25 anchors exact terms and identifiers, so a weak
general-English embedding model no longer sinks a query. A code-specific
embedding model (nomic-embed-code, jina-code) would still lift the *semantic*
arm further for purely conceptual queries — a model choice exposed via
`--embed-model`, not a wiring gap.

## Discovery

| Feature | Proof | What it demonstrates |
|---------|-------|----------------------|
| Strategy name resolution | `strategy-discovery.txt` | Built-ins resolve by name; unknown names fail with a helpful error. |

## Notes

- Built-in strategies (`monolithic`, `plan-exec`, `plan-critic-exec`,
  `wide-plan-exec`) are embedded in `pirs-rhai`; project `.pirs/strategies/`
  overrides and additions (e.g. `plan-oracle-exec`, `general-*`) resolve by name
  with project-then-home precedence.
- The extension packs are cataloged in `../extensions/README.md` and loaded by
  the `pirs-rhai` integration tests, counted in the 505 above.
