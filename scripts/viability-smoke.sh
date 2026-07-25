#!/usr/bin/env bash
# Cold-path viability smoke for pirs / pirs-claw (Phases 0–2).
# Does not require live Telegram, CDP, or LLM keys.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/home/driver/hero/build/target}"
SCRATCH="${SCRATCH:-/tmp/grok-goal-84daa1216f82/implementer}"
mkdir -p "$SCRATCH"

echo "== docs truth =="
{
  rg -n "Telegram-first|stub/thin|Skills Hub|LLM API key|allowlist|MCP connectors only|PLAN-FORWARD" \
    docs/ROADMAP.md docs/PRODUCTS.md docs/PLAN-FORWARD.md docs/VIABILITY-VS-HERMES-OPENCLAW.md \
    || true
} | tee "$SCRATCH/docs-sync.log"

echo "== unit: doctor honesty =="
cargo test -p pirs-tools --lib doctor_ -- --nocapture 2>&1 | tee "$SCRATCH/doctor-status.log"

echo "== unit: audit redaction + schedule status =="
{
  cargo test -p pirs-agent --lib redact_value -- --nocapture
  cargo test -p pirs-agent --lib tool_start_redacts -- --nocapture
  cargo test -p pirs-claw --lib schedule_status_lines -- --nocapture
  cargo test -p pirs-claw --lib schedule_fire_uses_timeout -- --nocapture
  cargo test -p pirs-claw --lib schedule_fail_count -- --nocapture
} 2>&1 | tee "$SCRATCH/spine-reliability.log"

echo "== package gates =="
cargo test -p pirs-tools -p pirs-agent -p pirs-claw --lib 2>&1 | tee "$SCRATCH/cargo-test.log"
cargo check -p pirs -p pirs-tools -p pirs-agent -p pirs-claw 2>&1 | tee "$SCRATCH/cargo-check.log"

echo "== optional CLI doctor (if binary built) =="
if [[ -x "$CARGO_TARGET_DIR/debug/pirs" ]]; then
  "$CARGO_TARGET_DIR/debug/pirs" --doctor 2>&1 | tee -a "$SCRATCH/doctor-status.log" || true
else
  cargo build -p pirs --bin pirs 2>&1 | tail -5
  "$CARGO_TARGET_DIR/debug/pirs" --doctor 2>&1 | tee -a "$SCRATCH/doctor-status.log" || true
fi

echo "viability-smoke: OK (logs under $SCRATCH)"
