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
import re
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

        # SWE-bench PASS_TO_PASS / FAIL_TO_PASS often include unittest *docstrings*
        # (e.g. "Tests the AddField operation.") which are not runnable ids and
        # bloat agent-discovery (django-14608: 211/394 keep-green were docstrings,
        # baseline alone 432s). Keep only ids that look like real test selectors.
        def looks_like_test_id(s: str) -> bool:
            s = s.strip()
            if not s or len(s) > 200:
                return False
            if "::" in s:  # pytest node id
                return True
            if s.startswith("test_") or ".test_" in s:
                return True
            # django/unittest label: "test_foo (module.Class)"
            if s.startswith("test_") or (s.startswith("test") and " (" in s):
                return True
            if re.match(r"^test\w* \(.*\)$", s):
                return True
            # bare sympy-style: test_mod
            if re.match(r"^test_[\w\[\],\-\.]+$", s):
                return True
            return False

        n_tg, n_kg = len(targets), len(keep_green)
        targets = [t for t in targets if looks_like_test_id(t)]
        keep_green = [t for t in keep_green if looks_like_test_id(t)]
        if len(targets) < n_tg or len(keep_green) < n_kg:
            logline(
                f"filtered non-test ids: targets {n_tg}->{len(targets)} "
                f"keep_green {n_kg}->{len(keep_green)}"
            )
        if not targets:
            # FAIL_TO_PASS is sometimes a unittest *docstring* title (django-15781:
            # "BaseCommand.create_parser() passes kwargs...") not a runnable id.
            # Recover real test_* names from the test_patch instead of re-injecting
            # the docstring (which always ReproFailed with turns=0).
            def targets_from_test_patch(diff: str) -> list[str]:
                out: list[str] = []
                cur_file = ""
                for line in diff.splitlines():
                    if line.startswith("+++ b/"):
                        cur_file = line[6:].strip()
                    # Newly added or context def lines (unified diff: leading
                    # +/-/space, then indentation, then def).
                    m = re.match(r"^[+ ]\s*def (test_\w+)\s*\(", line)
                    if not m:
                        continue
                    name = m.group(1)
                    if cur_file.endswith(".py"):
                        # Prefer pytest-style path::name; django runner also accepts
                        # short names via fuzzy match / module discovery.
                        out.append(f"{cur_file}::{name}")
                    else:
                        out.append(name)
                # de-dupe preserve order
                seen: set[str] = set()
                uniq = []
                for t in out:
                    if t not in seen:
                        seen.add(t)
                        uniq.append(t)
                return uniq

            tp = inst.get("test_patch") or ""
            recovered = targets_from_test_patch(tp)
            if recovered:
                targets = recovered
                logline(
                    f"WARNING: FAIL_TO_PASS had no runnable ids; "
                    f"recovered {len(targets)} from test_patch: {targets[:5]}"
                )
            else:
                targets = as_list(inst["FAIL_TO_PASS"])
                logline(
                    "WARNING: test-id filter removed all targets and test_patch "
                    "had no def test_*; using original FAIL_TO_PASS"
                )

        # Cap keep-green size. Huge PASS_TO_PASS lists (django-11019: 16 targets
        # + large media suite) burned the full 1800s agent timeout before a
        # fix landed. Official oracle still grades full P2P; harness only needs
        # a regression sample. Prefer tests that share a module prefix with a
        # FAIL_TO_PASS target.
        max_kg = int(os.environ.get("PIRS_MAX_KEEP_GREEN", "40"))
        if max_kg > 0 and len(keep_green) > max_kg:
            def kg_score(k: str) -> tuple:
                # Higher score = keep earlier. Prefer same module/file as targets.
                score = 0
                for t in targets:
                    if "::" in t and "::" in k and t.split("::")[0] == k.split("::")[0]:
                        score += 10
                    # django: "test_x (mod.Class)" — share parenthesized class/mod
                    if " (" in t and " (" in k:
                        tm = t[t.find("(") : t.find(")") + 1]
                        km = k[k.find("(") : k.find(")") + 1]
                        if tm and tm == km:
                            score += 10
                        elif tm and km and tm.split(".")[0] == km.split(".")[0]:
                            score += 5
                    if t.split("::")[-1].split("(")[0][:12] and t[:8] in k:
                        score += 1
                return (-score, k)

            ranked = sorted(keep_green, key=kg_score)
            keep_green = ranked[:max_kg]
            logline(
                f"capped keep_green {n_kg}->{len(keep_green)} (PIRS_MAX_KEEP_GREEN={max_kg})"
            )

        if strategy_script:
            sh(["docker", "cp", strategy_script, f"{cname}:/tmp/strategy.rhai"], stdout=log, stderr=log)

        # Use --flag=value so keep-green / target ids that start with "-"
        # (e.g. django docstring titles like "--squashed-name …") are not
        # re-parsed as CLI flags by clap.
        cmd = ["pirs-bench", "solve", "/testbed"]
        for t in targets:
            cmd.append(f"--target={t}")
        for k in keep_green:
            cmd.append(f"--keep-green={k}")
        cmd += [
            "--issue-file=/tmp/issue.md",
            f"--base-sha={head_sha}",
            f"--provider={provider}",
            f"--model={model}",
            f"--max-turns={max_turns}",
            "--out=/tmp/out.patch",
        ]
        if provider == "openai-compat":
            if not base_url:
                raise ValueError("base_url is required when provider='openai-compat'")
            cmd.append(f"--base-url={base_url}")
        if no_strategy:
            cmd += ["--no-strategy"]
        elif strategy_script:
            cmd.append("--strategy-script=/tmp/strategy.rhai")
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
