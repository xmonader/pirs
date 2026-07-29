#!/usr/bin/env bash
# Copy a live /tmp (or other) campaign into qa/bench-swebench-5x5/results_*
# so logs/patches/results are git-tracked.
#
# Usage:
#   ./land_results.sh /tmp/pirs-swe-lite-deepseek-strict-v2 results_deepseek_v4_flash_strict_v2_fifty
#   ./land_results.sh --all   # re-sync known active campaigns
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"

sync_one() {
  local src="$1" name="$2"
  local dest="$ROOT/$name"
  if [[ ! -d "$src" ]]; then
    echo "skip missing: $src" >&2
    return 0
  fi
  mkdir -p "$dest"
  rsync -a --delete --exclude '__pycache__' "$src/" "$dest/"
  local n
  n=$(find "$dest" -type f | wc -l)
  echo "landed $src -> $dest ($n files)"
}

if [[ "${1:-}" == "--all" ]]; then
  sync_one /tmp/pirs-swe-lite-deepseek-strict-v2 results_deepseek_v4_flash_strict_v2_fifty
  sync_one /tmp/pirs-swe-lite-deepseek-strict-naive-v2 results_deepseek_v4_flash_strict_naive_v2_fifty
  sync_one /tmp/pirs-swe-lite-deepseek-strict-verify-v2 results_deepseek_v4_flash_strict_verify_v2_fifty
  sync_one /tmp/pirs-swe-lite-pi-deepseek-strict results_pi_deepseek_v4_flash_strict_fifty
  sync_one /tmp/pirs-swe-lite-deepseek-strict results_deepseek_v4_flash_strict_fifty
  sync_one /tmp/pirs-swe-lite-deepseek-strict-verify results_deepseek_v4_flash_strict_verify_fifty
  exit 0
fi

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <src_dir> <results_DIRNAME>" >&2
  echo "   or: $0 --all" >&2
  exit 2
fi
sync_one "$1" "$2"
