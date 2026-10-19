# Ticket 14 — Observability

The server answers correctly; nothing lets an operator see *how*. This
ticket adds two statements: a query plan and exact service counters.

## This ticket's contract

1. **`EXPLAIN <select>`** answers a `ROWS` plan with one line per step.
   - An equality predicate on an indexed column names the index:
     a line containing `INDEX <table>.<index>` (for example
     `INDEX users.users_email`).
   - Without a usable index the plan names the scan: `SCAN <table>`
     (for example `SCAN users`).
   - The plan must not claim both for the same table: an indexed equality
     does not also scan it.
2. **`SHOW STATS`** answers `ROWS` with `name`/`value` rows (two columns,
   integer values) for exactly these counters:
   - `statements` — every non-empty statement line received, including
     `SHOW STATS` itself;
   - `selects`, `inserts`, `updates`, `deletes`, `commits`, `rollbacks` —
     by the statement's first keyword, counted whether or not the statement
     succeeds;
   - `errors` — every `ERR` answer;
   - `connections_current` — connections open right now;
   - `connections_total` — connections accepted since the server started.
3. **Deltas are exact.** After a known script (three `SELECT`s, two
   `INSERT`s, one `UPDATE`, one `DELETE`, a `COMMIT` and a `ROLLBACK`, one
   failing `SELECT`), each counter moves by exactly that amount, and the
   connection gauges follow opens and closes.

## What the suite does

The cumulative suites still pass; the scenario above runs once they do and
checks the plan shape and every counter delta.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
