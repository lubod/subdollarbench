# Ticket 12 (lite) — Concurrency: one global lock

This shorter ticket makes concurrent clients safe with **one global lock**:
every transaction takes it, so transactions run one at a time. It must be
*correct*; it does not need to let readers run while a writer is open.

## This ticket's contract

1. A transaction's effects are all-or-nothing to other connections. A
   conflicting transaction may answer `ERR busy` (the harness retries it),
   and it must then succeed.
2. No statement may block for more than 5 s.
3. 16 clients × **50** transactions of
   `BEGIN; SELECT v FROM t WHERE id = 1; UPDATE t SET v = <read + 1>
   WHERE id = 1; COMMIT` leave `v` at exactly **800**. No lost updates.
4. While transfers run, a reader that reads every account in one
   transaction always sees the same total.
5. A reader **may** block while another transaction is open — that is
   expected for a single global lock. (The full ticket additionally
   requires a reader to answer within 1 s while a writer is open; this
   variant does not.)

## What the suite does

The cumulative suites still pass, plus the lost-update and transfer
scenarios above.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
