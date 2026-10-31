#!/usr/bin/env bash
# One list of live SQL ladder proofs for gate.sh --fast and CI. These hold
# SQL_PERF_LIVE and run one at a time.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1
awk '/async fn |^[[:space:]]*fn /{match($0, /fn [a-z0-9_]+/); n=substr($0, RSTART+3, RLENGTH-3)}
     /SQL_PERF_LIVE\.lock/{print n}' src/pipeline.rs src/main.rs | sort -u
