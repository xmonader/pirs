#!/usr/bin/env python3
"""Re-test the disk-pressure hypothesis: matplotlib-23562, matplotlib-26011,
and scikit-learn-25570 all showed ReproFailed with bootstrap times 2-4x
slower than their same-repo baseline, right before the host disk hit
ENOSPC later in the same run. Now that disk space is free, re-run all 5
strategies on these 3 instances to see if they solve cleanly this time."""
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
    "matplotlib__matplotlib-23562",
    "matplotlib__matplotlib-26011",
    "scikit-learn__scikit-learn-25570",
]

if __name__ == "__main__":
    max_turns = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    timeout_s = int(sys.argv[2]) if len(sys.argv) > 2 else 2400
    concurrency = int(sys.argv[3]) if len(sys.argv) > 3 else 2
    out_dir = Path(sys.argv[4]) if len(sys.argv) > 4 else Path(__file__).parent / "results_disk_suspects"
    out_dir.mkdir(parents=True, exist_ok=True)

    jobs = [(iid, strat) for iid in INSTANCES for strat in STRATEGIES]
    print(f"disk-suspect rerun: {len(INSTANCES)} instances x {len(STRATEGIES)} strategies = {len(jobs)} runs, concurrency={concurrency}", flush=True)

    results = []
    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        futs = {}
        for iid, strat in jobs:
            fut = pool.submit(
                run_instance, iid, BASE_MODEL, max_turns, timeout_s, out_dir,
                strat["strategy_script"], strat["label"], strat["no_strategy"],
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

    summary_path = out_dir / "disk_suspects_summary.json"
    summary_path.write_text(json.dumps(results, indent=2))
    print(f"\nwrote summary to {summary_path}")

    by_instance = {}
    for r in results:
        by_instance.setdefault(r.get("id"), []).append(r)
    print("\n=== per instance ===")
    for iid, rs in by_instance.items():
        solved = sum(1 for r in rs if r.get("solved"))
        print(f"  {iid}: {solved}/{len(rs)} solved")
