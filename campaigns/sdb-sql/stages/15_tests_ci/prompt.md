# Ticket 15 — Tests and CI gate

The ladder built the database; nothing in it proves the code still works.
This ticket ships a **test suite** the harness can run offline, a coverage
signal, and tests that catch regressions.

## This ticket's contract

1. **`./test.sh` at the repo root**, executable and offline: it starts the
   server (or whatever pieces it needs), exercises it, and exits 0 on
   success, non-zero on failure. Two consecutive runs must both pass, each
   in under **120 s**.
2. **Coverage (measured by the harness)**: the harness runs your crate's
   own tests with `-C instrument-coverage` (`cargo test --release
   --offline --locked --tests`) and measures line coverage of `src/`,
   tests excluded, with llvm-tools. The gate is **80%**.
3. **Sensitivity** (the point of the ticket): in a scratch copy the harness
   reverses the **ticket-7 (aggregates)** and **ticket-8 (joins)** diffs.
   If a later ticket touched the same lines so the diff no longer applies
   cleanly, it falls back to the pre-ticket versions of the files those
   commits touched (the log says which). With either feature gone,
   `./test.sh` must **fail** — a suite that stays green without the feature
   is decoration, not a gate.
4. The suite must be deterministic: no network, no wall-clock races, no
   "sleep until it eventually works". The harness runs it twice and both
   runs must be green.
5. The sensitivity scratch copies may or may not include `.git` (the
   narrow diff check keeps it; the whole-file fallback does not), so
   `./test.sh` must be self-contained: do not shell out to `git`, and do not
   rely on files that only exist outside the repo.

## What the suite does

The cumulative suites still pass; the scenario above runs once they do and
then runs your `./test.sh` twice, measures the coverage, and reverts
tickets 7 and 8 in scratch copies to prove the tests catch those
regressions.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- `./test.sh` is the gate: keep it fast, offline and deterministic.
- When done: create `/workspace/READY`.
