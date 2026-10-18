# Ticket 6 — Transactions

Crash safety (ticket 5) makes single statements durable. This ticket adds
**transactions**: several statements that take effect together or not at
all, across a connection that is killed mid-flight.

## This ticket's contract

1. **`BEGIN` / `COMMIT` / `ROLLBACK`**, one transaction per connection.
2. **Atomicity**: inside a transaction, other connections must not see your
   changes until `COMMIT`; a `ROLLBACK` (or a crash) discards them entirely.
   A `COMMIT` survives everything.
3. **Errors inside a transaction** do not abort it: the failing statement
   changes nothing and the transaction is still open (matching SQLite).
4. **`ERR txn`** for a nested `BEGIN` and for `COMMIT`/`ROLLBACK` with no
   transaction active.
5. Statements outside a transaction stay autocommit, as before.

## What the acceptance suite does

The cumulative suites of the earlier tickets that passed still run, and the differential
generator now writes whole transactions (a `BEGIN`, one or more DML
statements, then `COMMIT` or `ROLLBACK`) plus the `txn` error probes, so
your answers must match the reference's for every one. On top:

- **Isolation**: a second connection never sees an uncommitted row; after
  `COMMIT` it does; after `ROLLBACK` it never does.
- **Kill with a transaction open**: the committed rows survive the kill -9
  and restart; the uncommitted ones leave no trace.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
