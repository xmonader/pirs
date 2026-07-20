#!/usr/bin/env python3
"""Re-run the 7 cells lost to a mid-batch 'No space left on device' error in
run_matrix2.py's second batch."""
import json
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from run_one import run_instance

PIRS_ROOT = Path("/home/driver/xmoncode/pirs")
BASE_MODEL = "deepseek-v4-flash"

STRAT_BY_LABEL = {
    "no-strategy": {"strategy_script": None, "no_strategy": True},
    "monolithic": {"strategy_script": None, "no_strategy": False},
    "plan-exec": {"strategy_script": str(PIRS_ROOT / ".pirs/strategies/plan-pro-exec-flash.rhai"), "no_strategy": False},
    "plan-critic-exec": {"strategy_script": str(PIRS_ROOT / ".pirs/strategies/plan-critic-exec-pro-flash.rhai"), "no_strategy": False},
    "wide-plan-exec": {"strategy_script": str(PIRS_ROOT / ".pirs/strategies/wide-plan-exec-pro-flash.rhai"), "no_strategy": False},
}

JOBS = [
    ("sympy__sympy-15346", "no-strategy"),
    ("sympy__sympy-15346", "monolithic"),
    ("sympy__sympy-15346", "plan-exec"),
    ("sympy__sympy-15346", "plan-critic-exec"),
    ("sympy__sympy-15346", "wide-plan-exec"),
    ("scikit-learn__scikit-learn-25570", "plan-critic-exec"),
    ("scikit-learn__scikit-learn-25570", "wide-plan-exec"),
]

if __name__ == "__main__":
    max_turns = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    timeout_s = int(sys.argv[2]) if len(sys.argv) > 2 else 2400
    concurrency = int(sys.argv[3]) if len(sys.argv) > 3 else 2
    out_dir = Path(sys.argv[4]) if len(sys.argv) > 4 else Path(__file__).parent / "results_matrix2"
    out_dir.mkdir(parents=True, exist_ok=True)

    print(f"rerun: {len(JOBS)} lost cells, concurrency={concurrency}", flush=True)
    results = []
    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        futs = {}
        for iid, label in JOBS:
            strat = STRAT_BY_LABEL[label]
            fut = pool.submit(
                run_instance, iid, BASE_MODEL, max_turns, timeout_s, out_dir,
                strat["strategy_script"], label, strat["no_strategy"],
            )
            futs[fut] = (iid, label)
        for fut in as_completed(futs):
            iid, label = futs[fut]
            try:
                r = fut.result()
            except Exception as e:
                r = {"id": iid, "label": label, "model": BASE_MODEL, "error": str(e), "solved": False}
            print(f"DONE {iid} [{label}]: solved={r.get('solved')} exit={r.get('exit_code')} elapsed={r.get('elapsed_s')}", flush=True)
            results.append(r)

    print(f"\nwrote {len(results)} results to {out_dir}")
