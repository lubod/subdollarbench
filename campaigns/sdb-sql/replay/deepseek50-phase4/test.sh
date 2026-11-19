#!/bin/sh
# Offline CI gate for sdb.
#
# Runs the crate's in-process regression tests (parser, planner, executor,
# storage, recovery) and the black-box end-to-end suite, which starts the
# real server on a private port and speaks the wire protocol.  The e2e suite
# lives in tests/ and deliberately does not import the crate, so it keeps
# working in scratch copies whose src/main.rs was rolled back.
set -eu

DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$DIR"

# The harness pre-builds, but test.sh must also stand on its own.
cargo build --release --offline --locked

LOG="${TMPDIR:-/tmp}/sdb-test-$$.log"
trap 'rm -f "$LOG"' EXIT INT TERM

# --tests runs both the unit tests and the integration/e2e test target.
# Test threads are kept modest so several server instances and stores do
# not compete for CPU on a small verifier box.
if ! cargo test --release --offline --locked --tests -- --test-threads=4 >"$LOG" 2>&1; then
    cat "$LOG"
    echo "test.sh: cargo test failed" >&2
    exit 1
fi
cat "$LOG"

# A suite that silently discovers no tests is not a gate.  The e2e case is
# the one that exercises the wire protocol and the whole feature ladder;
# it must actually have run and passed.
if ! grep -q 'end_to_end_gate .* ok' "$LOG"; then
    echo "test.sh: the end-to-end gate did not run" >&2
    exit 1
fi
exit 0
