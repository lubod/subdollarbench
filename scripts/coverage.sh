#!/bin/bash
# scripts/coverage.sh — the repo's 80%-lines campaign harness.
#
# Runs the same command the CI gate uses, prints the TOTAL line and the
# top-N files by missed lines (the work queue). Usage:
#   scripts/coverage.sh [top_n]   (default 15)
set -euo pipefail
cd "$(dirname "$0")/.."
TOP="${1:-15}"
OUT="$(mktemp /tmp/sdb-cov-XXXXXX.txt)"
# --all-targets matches .github/workflows/ci.yml (gate: --fail-under-lines 80).
cargo llvm-cov --all-targets --summary-only >"$OUT" 2>&1 || true
echo "== TOTAL =="
grep "^TOTAL" "$OUT" | awk '{printf "lines %s/%s (%.2f%%), regions %.2f%%, functions %.2f%%\n", $8-$9, $8, $10, $4, $7}'
echo
echo "== top $TOP by missed lines (file | missed/total) =="
grep -E "^[a-z_A-Z0-9/]+\.rs" "$OUT" \
  | awk '{print $9, $8, $1}' \
  | sort -k1 -n -r \
  | head -"$TOP" \
  | awk '{printf "  %6d / %-6d  %s\n", $1, $2, $3}'
echo
echo "full table: $OUT"
