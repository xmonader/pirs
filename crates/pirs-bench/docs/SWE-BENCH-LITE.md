# Running pirs-bench on SWE-bench Lite

A runbook for pointing the harness at real SWE-bench Lite instances. Read
[`../README.md`](../README.md) first for what the harness guarantees; this doc is
the operational how-to.

> **Set expectations.** The self-test corpus is tiny synthetic Python projects.
> Real SWE-bench Lite repos (django, sympy, astropy, …) are large and each needs
> a specific environment. The harness logic is proven; **environment setup is the
> fragile part** on real repos. Start with **one** instance, get it green end to
> end, then scale. Do not batch 300 instances on the first try.

## 1. Build

```bash
cargo build --release -p pirs-bench-runner   # binary: target/release/pirs-bench
```

The binary is named `pirs-bench` (the crate is `pirs-bench-runner`).

## 2. What one instance needs

The harness consumes a **local repo already checked out at the base commit, with
the test patch applied and committed**. That last part matters:

- SWE-bench's failing tests (`FAIL_TO_PASS`) are introduced by the instance's
  **`test_patch`**. Without it, the target tests don't exist and *reproduce-before-fix*
  correctly refuses to run.
- The harness's **test-file protection** reverts any agent edit to a test file
  back to `HEAD` before verifying. So `HEAD` must already contain the test patch
  — i.e. **commit the test patch** so the restore target is the real test, not an
  empty file. Checkout + apply + `git commit` is the required prep, not optional.

The agent must **not** get the gold `patch` — that's the answer. Only
`problem_statement`, the repo, and the target test ids go in.

## 3. Field mapping

| SWE-bench Lite field   | pirs-bench batch field | Notes |
|------------------------|------------------------|-------|
| `instance_id`          | `id`                   | free-form label |
| (local checkout path)  | `repo`                 | repo at base commit + committed test patch |
| `FAIL_TO_PASS`         | `targets`              | pytest node ids (`path/test_x.py::test_y`) |
| `PASS_TO_PASS`         | `keep_green`           | regression set; can be trimmed for speed |
| `problem_statement`    | `issue`                | the bug report the agent sees |
| `base_commit`          | `base_sha`             | baseline cache key; or omit to use `HEAD` |

`base_sha` is only a cache key for baseline reuse across instances at the same
checkout. If each instance is its own checkout, you can omit it (defaults to
`HEAD`). If you commit the test patch on top of `base_commit`, set `base_sha` to
that new commit or just omit it — never let two different trees share one key.

`PASS_TO_PASS` in SWE-bench is often hundreds of tests. The harness runs the full
regression set **once**, after a red→green flip — but that one run can dominate
wall-clock (check the `verify` line in the timing report). Trimming `keep_green`
to a representative subset trades regression coverage for speed; keep it full for
a real score.

## 4. Prep script (starting point)

This converts the HF dataset into (a) per-instance checkouts and (b) the JSONL
the `batch` command reads. It is a **starting point** — real runs need per-repo
dependency setup (step 5). Requires `pip install datasets`, `git`.

```python
#!/usr/bin/env python3
"""Prep SWE-bench Lite instances into local checkouts + a pirs-bench JSONL."""
import json, subprocess, sys
from pathlib import Path
from datasets import load_dataset

WORK = Path("/tmp/swebench-lite").resolve()
WORK.mkdir(parents=True, exist_ok=True)
N = int(sys.argv[1]) if len(sys.argv) > 1 else 1   # how many instances

def run(cmd, cwd):
    subprocess.run(cmd, cwd=cwd, check=True)

ds = load_dataset("princeton-nlp/SWE-bench_Lite", split="test")
lines = []
for row in ds.select(range(N)):
    iid = row["instance_id"]
    repo_dir = WORK / iid
    if not repo_dir.exists():
        url = f"https://github.com/{row['repo']}.git"
        run(["git", "clone", "--quiet", url, str(repo_dir)], cwd=WORK)
    run(["git", "checkout", "--quiet", "--force", row["base_commit"]], cwd=repo_dir)
    run(["git", "clean", "-fdq"], cwd=repo_dir)
    # Apply + commit the test patch so FAIL_TO_PASS tests exist and the
    # harness's test-file protection restores to the *real* test.
    (repo_dir / ".swebench_test.patch").write_text(row["test_patch"])
    run(["git", "apply", ".swebench_test.patch"], cwd=repo_dir)
    run(["git", "add", "-A"], cwd=repo_dir)
    run(["git", "-c", "user.email=b@b", "-c", "user.name=b",
         "commit", "--quiet", "-m", "test patch"], cwd=repo_dir)
    head = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repo_dir).decode().strip()
    lines.append({
        "id": iid,
        "repo": str(repo_dir),
        "targets": row["FAIL_TO_PASS"],
        "keep_green": row["PASS_TO_PASS"],
        "issue": row["problem_statement"],
        "base_sha": head,
    })

out = WORK / "instances.jsonl"
out.write_text("\n".join(json.dumps(l) for l in lines) + "\n")
print(f"wrote {len(lines)} instances -> {out}")
```

`FAIL_TO_PASS` / `PASS_TO_PASS` are JSON-encoded strings in some dataset dumps;
if you get a string instead of a list, `json.loads` it before assigning.

## 5. Environment setup (the hard part)

The harness bootstraps by extracting install commands from the repo's CI config
(the CI-oracle detector), then re-probing that `pytest` can collect tests. That
works when the repo installs cleanly with `pip install -e .` or a visible CI
recipe. It will **not** reproduce SWE-bench's canonical per-repo conda
environments. For a real evaluation you typically want the repo's deps already
installed into the active environment before running, e.g.:

```bash
cd /tmp/swebench-lite/<instance_id>
python -m venv .venv && . .venv/bin/activate
pip install -e .            # or the repo's documented dev-install
python -m pytest --collect-only -q <a FAIL_TO_PASS path>   # sanity: tests collect
```

If `--collect-only` can't import the package, the harness will bucket the
instance as `EnvSetup` — that's a real environment problem, not a harness bug.
Fix the env, then re-run.

## 6. Run

```bash
# Single instance — best for the first end-to-end check:
ANTHROPIC_API_KEY=… target/release/pirs-bench solve \
  /tmp/swebench-lite/<instance_id> \
  -t "sympy/core/tests/test_expr.py::test_x" \
  --issue-file /tmp/swebench-lite/<instance_id>/problem.md \
  --out /tmp/<instance_id>.patch

# Whole dataset:
ANTHROPIC_API_KEY=… target/release/pirs-bench batch \
  /tmp/swebench-lite/instances.jsonl --out-dir /tmp/swebench-patches/
```

Global knobs (all modes): `--model` (default `claude-opus-4-8`), `--provider`
(`anthropic` | `deepseek`), `--max-attempts` (verify-gated retries, default 3),
`--max-turns` (agent turns per attempt, default 40).

DeepSeek: `--provider deepseek --model deepseek-v4-flash` with `DEEPSEEK_API_KEY`.

## 7. Reading the output

Per instance (stderr):

```
session: turns=6 tools=7 [bash:2 read:2 edit:1 …]     # what the agent did
tokens by model: … TOTAL: in=… cache_r=… out=…        # cost
timing (total 41.75s): fix 85% · bootstrap 6% · …     # where the time went
  fix→tools: bash:4.10s edit:0.02s                    # tool time inside the fix
outcome: Accepted(…) | Failed(<bucket>)
```

At the end of a `batch`:

```
tasks: N  solved: k (…%)  scoped-only: …              # attribution histogram
tokens by model: … TOTAL: …
aggregate timing (total …s): fix … · bootstrap … · verify …
```

- **Accepted** = target flipped red→green, regression set stayed green,
  re-confirmed (not flaky). The patch is written to `--out` / `--out-dir`.
- **Failed(bucket)** tells you *where* it lost: `RunnerUndetected` (no test
  command found), `EnvSetup` (deps/import), `ReproFailed` (target didn't fail at
  base — usually a missing/wrong test patch), `FixNoFlip` (edit didn't turn it
  green), `Regressed` (broke a `keep_green` test), `Flaky` (flip didn't
  re-confirm).

## 8. Scoring against the reference harness

pirs-bench emits patches; it does **not** claim to be SWE-bench's official
scorer. To trust the numbers, feed the emitted `<id>.patch` files back through
the official SWE-bench evaluation harness and compare its verdict to
pirs-bench's `Accepted`/`Failed`. Divergences are the interesting signal — that
differential is the validation the harness still owes (see the README's honesty
note). Treat pirs-bench's `Accepted` as "passed *my* gate," and the official
harness as ground truth, until the two are shown to agree.
