#!/usr/bin/env bash
# R94 M1/M2: the one model-row/hash reader for the shell. The binary parses
# the run-time registry; shell callers never reimplement the float format.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${SDB_MODEL_ROW_BIN:-$ROOT/target/release/subdollarbench}"
exec "$BIN" model-row "$@"
