#!/usr/bin/env python3
"""Run a (instances x strategies) matrix through run_one.run_instance with
bounded concurrency. Each cell is one instance solved under one execution
mode; all modes use the same base --model (deepseek-v4-flash) and, where a
strategy has a non-executor phase, deepseek-v4-pro is pinned in via the
strategy script's own per-phase `model` field (not a CLI flag).
"""
import json
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from run_one import run_instance

PIRS_ROOT = Path("/home/driver/xmoncode/pirs")
BASE_MODEL = "deepseek-v4-flash"

STRATEGIES = [
    {"label": "no-strategy", "strategy_script": None, "no_strategy": True},
    {"label": "monolithic", "strategy_script": None, "no_strategy": False},
    {"label": "plan-exec", "strategy_script": str(PIRS_ROOT / ".pirs/strategies/plan-pro-exec-flash.rhai"), "no_strategy": False},
    {"label": "plan-critic-exec", "strategy_script": str(PIRS_ROOT / ".pirs/strategies/plan-critic-exec-pro-flash.rhai"), "no_strategy": False},
    {"label": "wide-plan-exec", "strategy_script": str(PIRS_ROOT / ".pirs/strategies/wide-plan-exec-pro-flash.rhai"), "no_strategy": False},
]

INSTANCES = [
    "astropy__astropy-6938",
    "matplotlib__matplotlib-23562",
    "pytest-dev__pytest-5221",
    "scikit-learn__scikit-learn-12471",
    "sphinx-doc__sphinx-7686",
]


if __name__ == "__main__":
    max_turns = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    timeout_s = int(sys.argv[2]) if len(sys.argv) > 2 else 2400
    concurrency = int(sys.argv[3]) if len(sys.argv) > 3 else 2
    out_dir = Path(sys.argv[4]) if len(sys.argv) > 4 else Path(__file__).parent / "results"
    out_dir.mkdir(parents=True, exist_ok=True)

    jobs = [(iid, strat) for iid in INSTANCES for strat in STRATEGIES]
    print(f"matrix: {len(INSTANCES)} instances x {len(STRATEGIES)} strategies = {len(jobs)} runs, concurrency={concurrency}", flush=True)

    results = []
    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        futs = {}
        for iid, strat in jobs:
            fut = pool.submit(
                run_instance,
                iid,
                BASE_MODEL,
                max_turns,
                timeout_s,
                out_dir,
                strat["strategy_script"],
                strat["label"],
                strat["no_strategy"],
            )
            futs[fut] = (iid, strat["label"])
        for fut in as_completed(futs):
            iid, label = futs[fut]
            try:
                r = fut.result()
            except Exception as e:
                r = {"id": iid, "label": label, "model": BASE_MODEL, "error": str(e), "solved": False}
            print(f"DONE {iid} [{label}]: solved={r.get('solved')} exit={r.get('exit_code')} elapsed={r.get('elapsed_s')}", flush=True)
            results.append(r)

    summary_path = out_dir / "matrix_summary.json"
    summary_path.write_text(json.dumps(results, indent=2))
    print(f"\nwrote summary to {summary_path}")

    by_strategy = {}
    for r in results:
        label = r.get("label", "?")
        by_strategy.setdefault(label, []).append(r.get("solved", False))
    print("\n=== solved / total by strategy ===")
    for label in [s["label"] for s in STRATEGIES]:
        outcomes = by_strategy.get(label, [])
        print(f"  {label}: {sum(1 for o in outcomes if o)}/{len(outcomes)}")
