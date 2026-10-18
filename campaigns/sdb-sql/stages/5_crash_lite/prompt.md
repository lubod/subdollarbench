# Ticket 5 (lite) — Crash safety: acked rows

Persistence (ticket 4) survives a *clean* stop. This lite ticket survives
**kill -9** for the rows it acknowledged. The full ticket also requires a
big `UPDATE` to be all-or-nothing under a kill and a torn tail to recover;
this variant does not test those two, so do not spend turns on them — the
cumulative suites of the earlier tickets that passed all still run.

## What this suite checks

1. **Acked rows survive a kill -9.** The harness sends single-row inserts,
   counts the acks, and kills the server right after one. After a restart,
   every acknowledged row must be present, and nothing half-written.
2. **A second restart changes nothing.**

## What "acked" means

Do not answer `OK` before the change is durable (fsync before the ack). A
kill right after the ack must not lose it. `--data` is the whole store, and
the process may be killed without warning.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `SPEC.md` in the workspace holds the whole contract so far.
- When done: create `/workspace/READY`.
