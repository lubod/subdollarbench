# Ticket 15 (lite) — Tests and CI gate, 60% coverage

The server works; this shorter ticket proves it with a test suite. It is
the same contract as ticket 15 with a **60% coverage** gate instead of 80%.

## This ticket's contract

1. **`./test.sh` at the repo root**, executable and offline: it starts the
   server (or whatever pieces it needs), exercises it, and exits 0 on
   success, non-zero on failure. Two consecutive runs must both pass, each
   in under **120 s**.
2. **Coverage**: Go projects are measured by the harness
   (`go test -coverprofile`); every other stack writes a line
   `coverage: NN.N%` into `coverage.txt`. The gate is **60%**.
3. **Sensitivity** is not relaxed: in a scratch copy the harness reverts the
   **ticket-7 (aggregates)** and **ticket-8 (joins)** changes. With either
   feature gone, `./test.sh` must **fail**.
4. The suite must be deterministic: no network, no wall-clock races, no
   "sleep until it eventually works". The harness runs it twice and both
   runs must be green.
5. The sensitivity runs use a scratch copy of the workspace **without
   `.git`**: `./test.sh` must be self-contained (do not shell out to
   `git`, and do not rely on files that only exist outside the repo).

## What the suite does

The cumulative suites still pass; the scenario above runs once they do and
then runs your `./test.sh` twice, checks the coverage signal, and reverts
tickets 7 and 8 in scratch copies to prove the tests catch those
regressions.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- `./test.sh` is the gate: keep it fast, offline and deterministic.
- When done: create `/workspace/READY`.
