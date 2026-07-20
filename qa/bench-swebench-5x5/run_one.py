#!/usr/bin/env python3
"""Run one SWE-bench-lite instance through pirs-bench inside its official
swebench eval docker image.

Steps: start container -> copy static pirs-bench binary in -> apply+commit the
test_patch (so FAIL_TO_PASS targets exist and test-file restore has a real
target) -> copy problem_statement in -> `pirs-bench solve` against /testbed
using the container's own already-installed conda env -> copy the patch (if
any) back out -> tear down the container.
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

BENCH_DIR = Path(__file__).parent
BINARY = "/home/driver/hero/build/target/x86_64-unknown-linux-musl/release/pirs-bench"
# docker exec does not source .bashrc, so the testbed conda env is never
# activated by default (PATH falls back to base miniconda). Every exec that
# needs the repo's installed deps (pytest, etc.) must set this explicitly.
TESTBED_PATH = (
    "/opt/miniconda3/envs/testbed/bin:/opt/miniconda3/condabin:/opt/miniconda3/bin:"
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
)


def image_for(instance_id: str) -> str:
    repo, num = instance_id.rsplit("-", 1)
    repo = repo.replace("/", "_").replace("__", "_1776_")
    return f"swebench/sweb.eval.x86_64.{repo}-{num}:latest"


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def run_instance(instance_id: str, model: str, max_turns: int, timeout_s: int, out_dir: Path,
                  strategy_script: str | None = None, label: str | None = None,
                  no_strategy: bool = False, provider: str = "deepseek",
                  base_url: str | None = None):
    inst = json.loads((BENCH_DIR / "instances" / f"{instance_id}.json").read_text())
    image = image_for(instance_id)
    tag = label or model
    cname = f"pirsbench-{instance_id.replace('/', '_')}-{tag}".replace("_", "-").lower()
    log_path = out_dir / f"{instance_id}.{tag}.log"
    patch_out = out_dir / f"{instance_id}.{tag}.patch"
    result = {"id": instance_id, "model": model, "label": tag, "image": image, "container": cname}

    log = open(log_path, "w")

    def logline(s):
        print(s, file=log, flush=True)

    try:
        logline(f"=== {instance_id} ({model}) start {time.time()} ===")
        subprocess.run(["docker", "rm", "-f", cname], capture_output=True)
        sh(["docker", "run", "-d", "--name", cname, image, "sleep", "infinity"], stdout=log, stderr=log)

        sh(["docker", "cp", BINARY, f"{cname}:/usr/local/bin/pirs-bench"], stdout=log, stderr=log)
        sh(["docker", "exec", cname, "chmod", "+x", "/usr/local/bin/pirs-bench"], stdout=log, stderr=log)

        # Write + apply + commit the test patch so FAIL_TO_PASS tests exist.
        test_patch_file = out_dir / f"{instance_id}.testpatch.diff"
        test_patch_file.write_text(inst["test_patch"])
        sh(["docker", "cp", str(test_patch_file), f"{cname}:/tmp/test.patch"], stdout=log, stderr=log)
        sh(["docker", "exec", cname, "bash", "-lc",
            "cd /testbed && git config user.email b@b.com && git config user.name bench && "
            "git apply --whitespace=fix /tmp/test.patch && git add -A && "
            "git commit -q -m 'apply swebench test patch'"], stdout=log, stderr=log)
        head_sha = subprocess.run(
            ["docker", "exec", cname, "git", "-C", "/testbed", "rev-parse", "HEAD"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        logline(f"head_sha={head_sha}")

        issue_file = out_dir / f"{instance_id}.issue.md"
        issue_file.write_text(inst["problem_statement"])
        sh(["docker", "cp", str(issue_file), f"{cname}:/tmp/issue.md"], stdout=log, stderr=log)

        def as_list(v):
            return json.loads(v) if isinstance(v, str) else v

        targets = as_list(inst["FAIL_TO_PASS"])
        keep_green = as_list(inst["PASS_TO_PASS"])

        if strategy_script:
            sh(["docker", "cp", strategy_script, f"{cname}:/tmp/strategy.rhai"], stdout=log, stderr=log)

        cmd = ["pirs-bench", "solve", "/testbed"]
        for t in targets:
            cmd += ["-t", t]
        for k in keep_green:
            cmd += ["-k", k]
        cmd += [
            "--issue-file", "/tmp/issue.md",
            "--base-sha", head_sha,
            "--provider", provider,
            "--model", model,
            "--max-turns", str(max_turns),
            "--out", "/tmp/out.patch",
        ]
        if provider == "openai-compat":
            if not base_url:
                raise ValueError("base_url is required when provider='openai-compat'")
            cmd += ["--base-url", base_url]
        if no_strategy:
            cmd += ["--no-strategy"]
        elif strategy_script:
            cmd += ["--strategy-script", "/tmp/strategy.rhai"]
        logline("cmd: " + " ".join(cmd))

        env_key_name = {
            "deepseek": "DEEPSEEK_API_KEY",
            "anthropic": "ANTHROPIC_API_KEY",
            "openai-compat": "CUSTOM_API_KEY",
        }[provider]
        api_key = os.environ[env_key_name]

        start = time.time()
        proc = subprocess.run(
            ["docker", "exec",
             "-e", f"{env_key_name}={api_key}",
             "-e", f"PATH={TESTBED_PATH}",
             "-e", "RUST_LOG=warn",
             cname] + cmd,
            capture_output=True, text=True, timeout=timeout_s,
        )
        elapsed = time.time() - start
        logline(proc.stdout)
        logline(proc.stderr)
        logline(f"exit_code={proc.returncode} elapsed_s={elapsed:.1f}")

        result["exit_code"] = proc.returncode
        result["elapsed_s"] = round(elapsed, 1)
        result["solved"] = proc.returncode == 0
        result["stderr_tail"] = "\n".join(proc.stderr.splitlines()[-40:])

        cp = subprocess.run(["docker", "cp", f"{cname}:/tmp/out.patch", str(patch_out)], capture_output=True, text=True)
        result["patch_copied"] = cp.returncode == 0
        if cp.returncode == 0:
            result["patch_bytes"] = patch_out.stat().st_size

    except subprocess.TimeoutExpired as e:
        result["exit_code"] = None
        result["timed_out"] = True
        result["solved"] = False
        logline(f"TIMEOUT after {timeout_s}s")
    except subprocess.CalledProcessError as e:
        result["error"] = f"setup step failed: {e}"
        result["solved"] = False
        logline(f"SETUP ERROR: {e}")
    finally:
        log.close()
        subprocess.run(["docker", "rm", "-f", cname], capture_output=True)

    result_path = out_dir / f"{instance_id}.{tag}.result.json"
    result_path.write_text(json.dumps(result, indent=2))
    return result


if __name__ == "__main__":
    instance_id = sys.argv[1]
    model = sys.argv[2]
    max_turns = int(sys.argv[3]) if len(sys.argv) > 3 else 40
    timeout_s = int(sys.argv[4]) if len(sys.argv) > 4 else 2400
    out_dir = Path(sys.argv[5]) if len(sys.argv) > 5 else BENCH_DIR / "results"
    strategy_script = sys.argv[6] if len(sys.argv) > 6 and sys.argv[6] != "-" else None
    label = sys.argv[7] if len(sys.argv) > 7 else None
    no_strategy = len(sys.argv) > 8 and sys.argv[8] == "1"
    out_dir.mkdir(parents=True, exist_ok=True)
    r = run_instance(instance_id, model, max_turns, timeout_s, out_dir, strategy_script, label, no_strategy)
    print(json.dumps(r, indent=2))
