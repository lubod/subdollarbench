# Ticket 5 — Crash safety

Persistence (ticket 4) survives a *clean* stop. This ticket survives a
**kill -9**: the harness may kill the process at any moment, without warning,
and after a restart everything the server acknowledged must still be there —
and nothing it did not acknowledge may be half-applied.

## What the acceptance suite does

The cumulative suite still runs (tickets 0-4), plus:

1. **Acked rows survive a kill -9.** The harness sends single-row inserts,
   counts the acks, and kills the server right after one. After a restart,
   exactly the acknowledged rows must be present — every acked one, and no
   half-written extra.
2. **A big `UPDATE` killed mid-flight is all-or-nothing.** 50k rows are
   updated in one statement; the kill lands while it runs. After a restart
   the column is uniformly old or uniformly new — never a mixture, and no
   row may disappear.
3. **A torn tail recovers.** The last bytes of your newest data file may be
   missing (a small partial trailing record — a few hundred bytes cut from
   the region your last commit grew, as after a power cut). On startup you
   must recover every statement committed *before* the damaged tail; the
   single most recent statement is allowed to be lost, older ones are not.
   Refusing to start, or losing older rows, fails.
4. **Two restarts in a row change nothing.**
5. **A restart is serving again within 20 seconds** for a store the size
   this campaign produces (tens of thousands of rows).

## What "acked" means for you

Do not answer `OK` before the change is durable: a kill right after the ack
must not lose it. Write with fsync, keep a recoverable log or WAL-like
structure, validate records (length or checksum) while recovering, and treat
a trailing partial record as absent. `--data` is still the whole store.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `SPEC.md` in the workspace holds the whole contract so far.
- When done: create `/workspace/READY`.
