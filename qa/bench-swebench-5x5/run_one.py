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


def want_trace() -> bool:
    """JSONL flight recorder on by default; set PIRS_TRACE=0 to disable."""
    v = os.environ.get("PIRS_TRACE", "1").strip().lower()
    return v not in ("0", "false", "no", "off")


def append_trace_flag(cmd: list[str]) -> list[str]:
    if want_trace() and not any(a.startswith("--trace") for a in cmd):
        return cmd + ["--trace=/tmp/trace.jsonl"]
    return cmd


def copy_trace_out(cname: str, out_path: Path, logline) -> bool:
    """Copy /tmp/trace.jsonl from the container if the recorder was enabled."""
    if not want_trace():
        return False
    cp = subprocess.run(
        ["docker", "cp", f"{cname}:/tmp/trace.jsonl", str(out_path)],
        capture_output=True,
        text=True,
    )
    ok = cp.returncode == 0 and out_path.exists() and out_path.stat().st_size > 0
    if ok:
        logline(f"trace_copied={out_path} bytes={out_path.stat().st_size}")
    else:
        logline(f"trace_copy_failed rc={cp.returncode} err={(cp.stderr or '').strip()[:200]}")
    return ok


def parse_token_stats(stderr: str) -> dict | None:
    """Extract in / cache_r / cache_w / out / reasoning / total from pirs-bench stderr.

    Looks for a TOTAL line like:
      TOTAL: in=12946 cache_r=106752 cache_w=0 out=1784 reasoning=490 total=121482 — $0.0129
    Falls back to the first model line with the same shape.
    """
    if not stderr:
        return None
    # Prefer TOTAL aggregate
    for line in stderr.splitlines():
        if "TOTAL:" not in line and not line.strip().startswith("TOTAL"):
            continue
        m = re.search(
            r"in=(\d+)\s+cache_r=(\d+)\s+cache_w=(\d+)\s+out=(\d+)\s+reasoning=(\d+)\s+total=(\d+)"
            r"(?:\s*[—\-]\s*\$([0-9.]+))?",
            line,
        )
        if m:
            cost = float(m.group(7)) if m.group(7) else None
            return {
                "in": int(m.group(1)),
                "cache_read": int(m.group(2)),
                "cache_write": int(m.group(3)),
                "out": int(m.group(4)),
                "reasoning": int(m.group(5)),
                "total": int(m.group(6)),
                "cost_usd": cost,
            }
    for line in stderr.splitlines():
        if "in=" not in line or "cache_r=" not in line:
            continue
        m = re.search(
            r"in=(\d+)\s+cache_r=(\d+)\s+cache_w=(\d+)\s+out=(\d+)\s+reasoning=(\d+)\s+total=(\d+)"
            r"(?:\s*[—\-]\s*\$([0-9.]+))?",
            line,
        )
        if m:
            cost = float(m.group(7)) if m.group(7) else None
            return {
                "in": int(m.group(1)),
                "cache_read": int(m.group(2)),
                "cache_write": int(m.group(3)),
                "out": int(m.group(4)),
                "reasoning": int(m.group(5)),
                "total": int(m.group(6)),
                "cost_usd": cost,
            }
    return None


def run_instance(instance_id: str, model: str, max_turns: int, timeout_s: int, out_dir: Path,
                  strategy_script: str | None = None, label: str | None = None,
                  no_strategy: bool = False, provider: str = "deepseek",
                  base_url: str | None = None, plan_model: str | None = None,
                  strategy: str | None = None,
                  raw_test_ids: bool | None = None,
                  hide_targets: bool | None = None,
                  strict: bool | None = None,
                  strict_verify: bool | None = None):
    """Run one instance.

    raw_test_ids: if True (or env PIRS_RAW_TEST_IDS=1), pass FAIL_TO_PASS /
    PASS_TO_PASS through unchanged — no looks_like_test_id filter and no
    test_patch name recovery. Default keeps the hygiene filter.

    hide_targets: if True (or env PIRS_FAIR=1 / PIRS_HIDE_TARGETS=1), pass
    --hide-targets so the agent prompt does NOT list FAIL_TO_PASS ids.
    Harness still grades with those ids (reproduce + verify). Fair mode.

    strict: if True (or env PIRS_STRICT=1), official-ish issue-only setup:
      1) agent runs on base commit with NO test_patch and --agent-only/--hide-targets
      2) then test_patch is applied and the model patch is graded with --check-patch
    Implies hide_targets; disables raw_test_ids spoon-feeding of any kind.

    strict_verify: if True (or env PIRS_STRICT_VERIFY=1), agent on base with no
      test_patch in workspace, but multi-attempt baseline/verify runs in a
      shadow worktree with test_patch (--shadow-test-patch). Opaque verdicts.
      Preferred over plain strict when both are set.
    """
    if strict is None:
        strict = os.environ.get("PIRS_STRICT", "").strip().lower() in (
            "1", "true", "yes", "on",
        )
    if strict_verify is None:
        strict_verify = os.environ.get("PIRS_STRICT_VERIFY", "").strip().lower() in (
            "1", "true", "yes", "on",
        )
    if raw_test_ids is None:
        raw_test_ids = os.environ.get("PIRS_RAW_TEST_IDS", "").strip().lower() in (
            "1", "true", "yes", "on",
        )
    if hide_targets is None:
        hide_targets = (
            os.environ.get("PIRS_FAIR", "").strip().lower() in ("1", "true", "yes", "on")
            or os.environ.get("PIRS_HIDE_TARGETS", "").strip().lower()
            in ("1", "true", "yes", "on")
        )
    if strict_verify:
        strict = False  # shadow-verify replaces agent-only strict
        hide_targets = True
        raw_test_ids = False
    if strict:
        hide_targets = True
        raw_test_ids = False
    inst = json.loads((BENCH_DIR / "instances" / f"{instance_id}.json").read_text())
    image = image_for(instance_id)
    tag = label or model
    cname = f"pirsbench-{instance_id.replace('/', '_')}-{tag}".replace("_", "-").lower()
    log_path = out_dir / f"{instance_id}.{tag}.log"
    patch_out = out_dir / f"{instance_id}.{tag}.patch"
    result = {
        "id": instance_id,
        "model": model,
        "plan_model": plan_model,
        "label": tag,
        "image": image,
        "container": cname,
        "strategy": strategy,
        "raw_test_ids": raw_test_ids,
        "hide_targets": hide_targets,
        "fair": hide_targets and not strict and not strict_verify,
        "strict": strict,
        "strict_verify": strict_verify,
    }

    log = open(log_path, "w")

    def logline(s):
        print(s, file=log, flush=True)

    try:
        logline(f"=== {instance_id} ({model}) start {time.time()} ===")
        subprocess.run(["docker", "rm", "-f", cname], capture_output=True)
        sh(["docker", "run", "-d", "--name", cname, image, "sleep", "infinity"], stdout=log, stderr=log)

        sh(["docker", "cp", BINARY, f"{cname}:/usr/local/bin/pirs-bench"], stdout=log, stderr=log)
        sh(["docker", "exec", cname, "chmod", "+x", "/usr/local/bin/pirs-bench"], stdout=log, stderr=log)

        # Always configure git; base_sha is the image checkout (pre test_patch).
        sh(["docker", "exec", cname, "bash", "-lc",
            "cd /testbed && git config user.email b@b.com && git config user.name bench"],
           stdout=log, stderr=log)
        base_sha = subprocess.run(
            ["docker", "exec", cname, "git", "-C", "/testbed", "rev-parse", "HEAD"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        logline(f"base_sha={base_sha}")

        test_patch_file = out_dir / f"{instance_id}.testpatch.diff"
        test_patch_file.write_text(inst["test_patch"])
        sh(["docker", "cp", str(test_patch_file), f"{cname}:/tmp/test.patch"], stdout=log, stderr=log)

        issue_file = out_dir / f"{instance_id}.issue.md"
        issue_file.write_text(inst["problem_statement"])
        sh(["docker", "cp", str(issue_file), f"{cname}:/tmp/issue.md"], stdout=log, stderr=log)

        def as_list(v):
            return json.loads(v) if isinstance(v, str) else v

        targets = as_list(inst["FAIL_TO_PASS"])
        keep_green = as_list(inst["PASS_TO_PASS"])
        n_tg, n_kg = len(targets), len(keep_green)

        if raw_test_ids:
            # Ablation: no looks_like_test_id, no test_patch recovery.
            logline(
                f"raw_test_ids=1: using FAIL_TO_PASS/PASS_TO_PASS as-is "
                f"(targets={n_tg} keep_green={n_kg})"
            )
        else:
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
                    """Pull test_* names the patch actually touches.

                    Keep a def only if a `+` body line appears before the next def,
                    so we do not grab the following untouched test as FAIL_TO_PASS.
                    """
                    out: list[str] = []
                    cur_file = ""
                    lines = diff.splitlines()
                    i = 0
                    while i < len(lines):
                        line = lines[i]
                        if line.startswith("+++ b/"):
                            cur_file = line[6:].strip()
                            i += 1
                            continue
                        m = re.match(r"^[+ ]\s*def (test_\w+)\s*\(", line)
                        if not m:
                            i += 1
                            continue
                        name = m.group(1)
                        touched = line.startswith("+")
                        j = i + 1
                        while j < len(lines):
                            nxt = lines[j]
                            if re.match(r"^[+ ]\s*def (test_\w+)\s*\(", nxt):
                                break
                            if nxt.startswith("+++ b/") or nxt.startswith("diff --git"):
                                break
                            if nxt.startswith("+") and not nxt.startswith("+++"):
                                touched = True
                                break
                            j += 1
                        if touched:
                            tid = (
                                f"{cur_file}::{name}"
                                if cur_file.endswith(".py")
                                else name
                            )
                            if tid not in out:
                                out.append(tid)
                        i += 1
                    return out

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

        env_key_name = {
            "deepseek": "DEEPSEEK_API_KEY",
            "anthropic": "ANTHROPIC_API_KEY",
            "openai-compat": "CUSTOM_API_KEY",
        }[provider]
        api_key = os.environ[env_key_name]

        def docker_exec(cmd, timeout=timeout_s):
            return subprocess.run(
                ["docker", "exec",
                 "-e", f"{env_key_name}={api_key}",
                 "-e", f"PATH={TESTBED_PATH}",
                 "-e", "RUST_LOG=warn",
                 cname] + cmd,
                capture_output=True, text=True, timeout=timeout,
            )

        start = time.time()

        if strict_verify:
            # ── STRICT+VERIFY: blind tests, shadow worktree multi-attempt gate ─
            logline(
                "strict_verify=1: agent on base (NO test_patch in workspace); "
                "after each attempt grade in worktree via --shadow-test-patch "
                "(opaque verdicts, full baseline/verify stack)"
            )
            cmd = [
                "pirs-bench", "solve", "/testbed",
                "--shadow-test-patch=/tmp/test.patch",
                "--hide-targets",
                "--issue-file=/tmp/issue.md",
                f"--base-sha={base_sha}",
                f"--provider={provider}",
                f"--model={model}",
                f"--max-turns={max_turns}",
                "--out=/tmp/out.patch",
            ]
            for t in targets:
                cmd.append(f"--target={t}")
            for k in keep_green:
                cmd.append(f"--keep-green={k}")
            if provider == "openai-compat":
                if not base_url:
                    raise ValueError("base_url is required when provider='openai-compat'")
                cmd.append(f"--base-url={base_url}")
            if plan_model:
                cmd.append(f"--plan-model={plan_model}")
            if no_strategy:
                cmd += ["--no-strategy"]
            elif strategy_script:
                cmd.append("--strategy-script=/tmp/strategy.rhai")
            elif strategy:
                cmd.append(f"--strategy={strategy}")
            cmd = append_trace_flag(cmd)
            logline("shadow_cmd: " + " ".join(cmd))
            proc = docker_exec(cmd)
            elapsed = time.time() - start
            logline(proc.stdout)
            logline(proc.stderr)
            logline(f"exit_code={proc.returncode} elapsed_s={elapsed:.1f}")
            result["exit_code"] = proc.returncode
            result["elapsed_s"] = round(elapsed, 1)
            result["solved"] = proc.returncode == 0
            result["stderr_tail"] = "\n".join(proc.stderr.splitlines()[-100:])
            tokens = parse_token_stats(proc.stderr)
            if tokens:
                result["tokens"] = tokens
            cp = subprocess.run(
                ["docker", "cp", f"{cname}:/tmp/out.patch", str(patch_out)],
                capture_output=True, text=True,
            )
            result["patch_copied"] = cp.returncode == 0 and patch_out.exists()
            if result["patch_copied"] and patch_out.exists():
                result["patch_bytes"] = patch_out.stat().st_size
            trace_out = out_dir / f"{instance_id}.{tag}.trace.jsonl"
            result["trace_copied"] = copy_trace_out(cname, trace_out, logline)

        elif strict:
            # ── STRICT: agent never sees test_patch ──────────────────────────
            # Phase 1: issue-only agent on base commit → model patch
            logline(
                "strict=1: agent-only on base (NO test_patch); "
                "then apply test_patch + grade with --check-patch"
            )
            agent_cmd = [
                "pirs-bench", "solve", "/testbed",
                "--agent-only",
                "--hide-targets",
                "--issue-file=/tmp/issue.md",
                f"--base-sha={base_sha}",
                f"--provider={provider}",
                f"--model={model}",
                f"--max-turns={max_turns}",
                "--out=/tmp/out.patch",
            ]
            if provider == "openai-compat":
                if not base_url:
                    raise ValueError("base_url is required when provider='openai-compat'")
                agent_cmd.append(f"--base-url={base_url}")
            if plan_model:
                agent_cmd.append(f"--plan-model={plan_model}")
            if no_strategy:
                agent_cmd += ["--no-strategy"]
            elif strategy_script:
                agent_cmd.append("--strategy-script=/tmp/strategy.rhai")
            elif strategy:
                agent_cmd.append(f"--strategy={strategy}")
            agent_cmd = append_trace_flag(agent_cmd)
            logline("agent_cmd: " + " ".join(agent_cmd))
            proc_agent = docker_exec(agent_cmd)
            logline(proc_agent.stdout)
            logline(proc_agent.stderr)
            logline(f"agent_exit={proc_agent.returncode}")
            tokens = parse_token_stats(proc_agent.stderr)
            if tokens:
                result["tokens"] = tokens
            # Keep more lines so phase.start/end SPARK/EMBER are visible in tails.
            result["stderr_tail"] = "\n".join(proc_agent.stderr.splitlines()[-200:])
            trace_out = out_dir / f"{instance_id}.{tag}.trace.jsonl"
            result["trace_copied"] = copy_trace_out(cname, trace_out, logline)

            cp = subprocess.run(
                ["docker", "cp", f"{cname}:/tmp/out.patch", str(patch_out)],
                capture_output=True, text=True,
            )
            result["patch_copied"] = (
                cp.returncode == 0 and patch_out.exists() and patch_out.stat().st_size > 0
            )
            if result["patch_copied"]:
                result["patch_bytes"] = patch_out.stat().st_size
                # Ensure clean base, apply test_patch only, then grade model patch.
                sh(["docker", "exec", cname, "git", "-C", "/testbed", "reset", "--hard", base_sha],
                   stdout=log, stderr=log)
                sh(["docker", "exec", cname, "bash", "-lc",
                    "cd /testbed && git apply --whitespace=fix /tmp/test.patch && "
                    "git add -A && git commit -q -m 'apply swebench test patch'"],
                   stdout=log, stderr=log)
                # Re-copy model patch into container (agent-only resets the tree)
                sh(["docker", "cp", str(patch_out), f"{cname}:/tmp/out.patch"],
                   stdout=log, stderr=log)

                check_cmd = ["pirs-bench", "solve", "/testbed", "--check-patch=/tmp/out.patch"]
                for t in targets:
                    check_cmd.append(f"--target={t}")
                for k in keep_green:
                    check_cmd.append(f"--keep-green={k}")
                logline("check_cmd: " + " ".join(check_cmd))
                proc = docker_exec(check_cmd, timeout=max(600, timeout_s // 2))
                logline(proc.stdout)
                logline(proc.stderr)
                logline(f"check_exit={proc.returncode}")
                result["exit_code"] = proc.returncode
                result["solved"] = proc.returncode == 0
                result["stderr_tail"] = (
                    result.get("stderr_tail", "")
                    + "\n--- check ---\n"
                    + "\n".join(proc.stderr.splitlines()[-40:])
                )
            else:
                result["exit_code"] = proc_agent.returncode or 1
                result["solved"] = False
                result["error"] = "strict: agent produced no patch"
                logline("strict: no agent patch — skip check")
            result["elapsed_s"] = round(time.time() - start, 1)
        else:
            # ── Normal / fair: test_patch in tree before agent ────────────────
            sh(["docker", "exec", cname, "bash", "-lc",
                "cd /testbed && git apply --whitespace=fix /tmp/test.patch && "
                "git add -A && git commit -q -m 'apply swebench test patch'"],
               stdout=log, stderr=log)
            head_sha = subprocess.run(
                ["docker", "exec", cname, "git", "-C", "/testbed", "rev-parse", "HEAD"],
                capture_output=True, text=True, check=True,
            ).stdout.strip()
            logline(f"head_sha={head_sha} (after test_patch)")

            # Use --flag=value so keep-green / target ids that start with "-"
            # are not re-parsed as CLI flags by clap.
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
            if hide_targets:
                cmd.append("--hide-targets")
                logline(
                    "fair/hide_targets=1: agent prompt omits FAIL_TO_PASS ids; "
                    "harness still verifies against them (test_patch pre-applied)"
                )
            if provider == "openai-compat":
                if not base_url:
                    raise ValueError("base_url is required when provider='openai-compat'")
                cmd.append(f"--base-url={base_url}")
            if plan_model:
                cmd.append(f"--plan-model={plan_model}")
            if no_strategy:
                cmd += ["--no-strategy"]
            elif strategy_script:
                cmd.append("--strategy-script=/tmp/strategy.rhai")
            elif strategy:
                cmd.append(f"--strategy={strategy}")
            cmd = append_trace_flag(cmd)
            logline("cmd: " + " ".join(cmd))

            proc = docker_exec(cmd)
            elapsed = time.time() - start
            logline(proc.stdout)
            logline(proc.stderr)
            logline(f"exit_code={proc.returncode} elapsed_s={elapsed:.1f}")

            result["exit_code"] = proc.returncode
            result["elapsed_s"] = round(elapsed, 1)
            result["solved"] = proc.returncode == 0
            result["stderr_tail"] = "\n".join(proc.stderr.splitlines()[-200:])
            tokens = parse_token_stats(proc.stderr)
            if tokens:
                result["tokens"] = tokens

            cp = subprocess.run(
                ["docker", "cp", f"{cname}:/tmp/out.patch", str(patch_out)],
                capture_output=True, text=True,
            )
            result["patch_copied"] = cp.returncode == 0
            if cp.returncode == 0 and patch_out.exists():
                result["patch_bytes"] = patch_out.stat().st_size
            trace_out = out_dir / f"{instance_id}.{tag}.trace.jsonl"
            result["trace_copied"] = copy_trace_out(cname, trace_out, logline)

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
