#!/usr/bin/env bash
# R85 F2: write runs/billed-models.json from the .47 campaigns' key
# readings. Read-only over the runs; the board reads the output file.
#
# Usage: scripts/backfill-billed.sh [--runs DIR] [--after SPEND]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cargo run --quiet --bin backfill-billed -- "$@"
