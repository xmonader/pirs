#!/usr/bin/env bash
# shrimpswe for pirs — port of hero_shrimp/support/extbench/shrimpswe.
#
# Blind prompt → agent edits workspace → hidden bun:test check + own-tests guard.
# No LLM judge. No shrimp daemon.
#
#   ./run.sh [task...]
#   MODEL=deepseek-v4-flash PLAN_MODEL=deepseek-v4-pro STRATEGY=weak-drive ./run.sh multi-site
#
# Env:
#   PIRS          path to pirs binary (default: release build)
#   MODEL         executor model (default deepseek-v4-flash)
#   PLAN_MODEL    planner model (default deepseek-v4-pro; ignored for naive)
#   STRATEGY      weak-drive | plan-exec | monolithic | naive | …
#   AUTONOMY      plan|edit|full (default **full** — bash/tests must work)
#   CAP           wall-clock seconds per task (default 1800)
#   MAX_TURNS     agent max turns (default 40)
#   OUT           output directory
set -u
export PATH="${HOME}/.hermes/node/bin:${HOME}/.bun/bin:${PATH}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OUT="${OUT:-$HERE/runs/pirs-$(date +%Y%m%d-%H%M%S)}"
CAP="${CAP:-1800}"
MAX_TURNS="${MAX_TURNS:-40}"
MODEL="${MODEL:-deepseek-v4-flash}"
PLAN_MODEL="${PLAN_MODEL:-deepseek-v4-pro}"
STRATEGY="${STRATEGY:-weak-drive}"
# Bench must be able to run tests — default edit autonomy blocked bash (false fails).
AUTONOMY="${AUTONOMY:-full}"
PIRS="${PIRS:-${CARGO_TARGET_DIR:-$HOME/hero/build/target}/release/pirs}"
if [ ! -x "$PIRS" ]; then
  PIRS="$ROOT/target/release/pirs"
fi
if [ ! -x "$PIRS" ]; then
  echo "need a pirs binary (build: cargo build -p pirs --release)" >&2
  exit 1
fi

# Load keys if present (do not print them).
set -a
[ -f "$HOME/.pirs/secrets.env" ] && . "$HOME/.pirs/secrets.env"
[ -f "$HOME/hero/config/shrimp/secrets.env" ] && . "$HOME/hero/config/shrimp/secrets.env"
set +a

# Prefer CLI model over ~/.pirs/config.toml default when present.
export PIRS_MODEL="$MODEL"
# Never let config silently swap the executor.
export PIRS_AUTONOMY="$AUTONOMY"

TASKS=("$@")
if [ ${#TASKS[@]} -eq 0 ]; then
  # Default: capability suite (skip waiter internals — shrimp ledger only).
  TASKS=(multi-site red-herring needle broken-env moving-target)
fi

# Protect the shared template: agents previously SEARCH/REPLACE'd absolute paths
# under fixture/ and poisoned later tasks. Workspace copies are still writable.
if [ -d "$HERE/fixture" ]; then
  chmod -R a-w "$HERE/fixture" 2>/dev/null || true
  # Keep dirs executable so we can traverse/copy.
  find "$HERE/fixture" -type d -exec chmod a+x {} + 2>/dev/null || true
fi

mkdir -p "$OUT"
echo "shrimpswe/pirs | ${#TASKS[@]} task(s) | model=$MODEL plan=$PLAN_MODEL strategy=$STRATEGY autonomy=$AUTONOMY"
echo "  pirs: $PIRS"
echo "  out:  $OUT"
echo

build_pirs_args() {
  local ws=$1
  # Force model on the CLI *and* via env; --autonomy full unlocks bash.
  # Work only in -C workspace; prompt also forbids editing outside.
  # --cwd and short -C both chdir (CLI maps -C → cwd). Prefer --cwd in scripts.
  pirs_args=(
    --cwd "$ws"
    --model "$MODEL"
    --autonomy "$AUTONOMY"
    --max-turns "$MAX_TURNS"
    --no-extensions
  )

  case "${STRATEGY}" in
    naive|none|"")
      ;;
    *)
      pirs_args+=(--strategy "$STRATEGY")
      if [ -n "${PLAN_MODEL}" ]; then
        pirs_args+=(--plan-model "$PLAN_MODEL")
      fi
      ;;
  esac
}

wrap_prompt() {
  # Remind the agent of cwd isolation (absolute-path edits to ../fixture were a bug).
  local body=$1
  local id=${2:-}
  local prefix="IMPORTANT: The repository is ONLY the current working directory. Read and edit files relative to cwd (e.g. src/…, tests/…). Do NOT open or write absolute paths outside this workspace."
  # moving-target class: pull-based streaming rules (skill streaming-export).
  if [ "$id" = "moving-target" ]; then
    prefix="$prefix

STREAMING / LARGE-EXPORT RULES (must hold in the FINAL design):
1. Return a pull-based iterable of chunks/lines for the export API when the task is streaming/OOM/large — not only a fully-built string.
2. First pull of the result must not fully consume a one-shot input generator (touching only the first chunk must leave most source rows unpulled).
3. Do not do a full first pass over a one-shot iterator only for TOTAL then join a giant string — one pass with a running total, or skill streaming-export."
  fi
  printf '%s\n\n%s\n' "$prefix" "$body"
}

pass=0
for id in "${TASKS[@]}"; do
  if [ ! -f "$HERE/prompts/$id.md" ]; then
    echo "  unknown task: $id" >&2
    continue
  fi
  ws="$OUT/$id/workspace"
  mkdir -p "$OUT/$id"
  rm -rf "$ws"
  # Copy template (may be read-only bits) then ensure workspace is writable.
  mkdir -p "$ws"
  cp -a "$HERE/fixture/." "$ws/"
  chmod -R u+w "$ws" 2>/dev/null || true
  (cd "$ws" && bun install --silent >/dev/null 2>&1
   git init -q
   git add -A
   git -c user.email=bench@local -c user.name=bench commit -qm base) || true
  base=$(git -C "$ws" rev-parse HEAD 2>/dev/null || echo "")

  t0=$(date +%s)
  prompt=$(wrap_prompt "$(cat "$HERE/prompts/$id.md")" "$id")
  build_pirs_args "$ws"
  set +e
  timeout "$CAP" "$PIRS" "${pirs_args[@]}" "$prompt" \
    >"$OUT/$id/agent.log" 2>&1
  rc1=$?
  set -e

  # Second turn on same workspace (moving-target, waiter, cross-session).
  if [ -f "$HERE/prompts/$id.2.md" ]; then
    if [ -f "$HERE/checks/$id.fresh-workspace" ]; then
      mv "$ws" "$OUT/$id/workspace-turn1"
      mkdir -p "$ws"
      cp -a "$HERE/fixture/." "$ws/"
      chmod -R u+w "$ws" 2>/dev/null || true
      (cd "$ws" && bun install --silent >/dev/null 2>&1
       git init -q
       git add -A
       git -c user.email=bench@local -c user.name=bench commit -qm base) || true
      base=$(git -C "$ws" rev-parse HEAD 2>/dev/null || echo "")
    fi
    prompt2=$(wrap_prompt "$(cat "$HERE/prompts/$id.2.md")" "$id")
    build_pirs_args "$ws"
    set +e
    timeout "$CAP" "$PIRS" "${pirs_args[@]}" "$prompt2" \
      >"$OUT/$id/agent.2.log" 2>&1
    rc2=$?
    set -e
  else
    rc2=0
  fi
  el=$(( $(date +%s) - t0 ))

  if [ -n "$base" ]; then
    git -C "$ws" diff "$base" -- . \
      ':(exclude,glob)**/node_modules/**' \
      ':(exclude,glob)**/.pirs/**' \
      >"$OUT/$id/model.patch" 2>/dev/null || true
  else
    : >"$OUT/$id/model.patch"
  fi
  sz=$(wc -c <"$OUT/$id/model.patch" | tr -d ' ')
  if [ "${sz:-0}" -gt 262144 ]; then
    echo "    WARNING: $id patch is $sz bytes — suspect pollution" >&2
  fi

  # Hidden acceptance (only now).
  cp "$HERE/checks/$id.test.ts" "$ws/tests/"
  set +e
  (cd "$ws" && bun test "tests/$id.test.ts" >"$OUT/$id/check.log" 2>&1)
  set -e
  tot=$(grep -oE "^ [0-9]+ (pass|fail)" "$OUT/$id/check.log" 2>/dev/null | tr '\n' ' ' || true)
  ok=0
  if grep -qE " 0 fail" "$OUT/$id/check.log" 2>/dev/null \
    && ! grep -qE " [1-9][0-9]* fail" "$OUT/$id/check.log" 2>/dev/null; then
    ok=1
  fi

  set +e
  (cd "$ws" && bun test tests/money.test.ts tests/csv.test.ts tests/reports.test.ts \
    >"$OUT/$id/own-tests.log" 2>&1)
  if [ $? -eq 0 ]; then own=clean; else own=REGRESSED; fi
  set -e

  mech=ok
  if [ -f "$HERE/checks/$id.ledger" ]; then
    mech=n/a-pirs
  fi

  # Detect model actually used (from usage footer).
  used=$(rg -oN '^\s+([a-zA-Z0-9._/-]+) \([0-9]+ call' "$OUT/$id/agent.log" 2>/dev/null | head -1 | sed 's/.*(//;s/ call.*//' || true)
  used=${used:-?}
  # Prefer the "qwen… (N calls)" style line
  used_line=$(rg -N '^\s+\S+ \([0-9]+ calls?\)' "$OUT/$id/agent.log" 2>/dev/null | tail -1 | awk '{print $1}' || true)
  [ -n "$used_line" ] && used=$used_line

  if [ "$ok" = 1 ] && [ "$own" = clean ]; then
    v=FIXED
    pass=$((pass + 1))
  else
    v=NOT-FIXED
  fi
  printf '  %-14s %-10s %-22s own-tests=%s model_used=%s agent_rc=%s/%s %ds\n' \
    "$id" "$v" "${tot:--}" "$own" "$used" "$rc1" "$rc2" "$el"
done

# Fixture must still match expected bugs (no pollution).
if ! grep -q 'r.label.length > 0' "$HERE/fixture/src/handlers/h9.ts" 2>/dev/null; then
  echo "WARNING: fixture h9 looks polluted (expected label.length bug)" >&2
fi
if grep -q '^>' "$HERE/fixture/src/rows.ts" 2>/dev/null; then
  echo "WARNING: fixture rows.ts has stray '>' — polluted" >&2
fi

echo
echo "  $pass/${#TASKS[@]} fixed  (strategy=$STRATEGY model=$MODEL plan=$PLAN_MODEL autonomy=$AUTONOMY)"
echo "$pass ${#TASKS[@]} $STRATEGY $MODEL $PLAN_MODEL $AUTONOMY" >"$OUT/SCORE.txt"
[ "$pass" -eq "${#TASKS[@]}" ]
