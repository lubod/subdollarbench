# Ticket 7 — Delays and redrive

Two operator requests: schedule work for later, and give a dead job
another life after a fix.

## Contract additions

1. **Delayed enqueue**: `ENQ <queue> <payload> IN <delay_ms>` -> `+OK <id>`.
   The job is not leasable until the delay has passed; afterwards it is a
   normal job (first lease reports `attempts` = 1).
2. **`REDRIVE <queue> <id>`** -> `+OK`: moves the dead-letter job `<id>`
   from `<queue>.dead` back to `<queue>` with `attempts` reset to 0 (so
   its next lease reports 1). Unknown ids answer `-ERR ...`. After a
   redrive the job is no longer in `<queue>.dead`.
3. Everything earlier stays green.

## What the acceptance suite does

Enqueues with a 1.5s delay, proves it is invisible before the delay and
leasable after; then poisons a job into the DLQ and proves `REDRIVE`
returns it to the live queue with `attempts` reset, and that an unknown id
errors.
