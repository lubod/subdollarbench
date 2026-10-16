# Ticket 9 — Constraints

Joins (ticket 8) read many tables. This ticket makes the schema **mean
something**: primary keys, uniqueness, required columns, defaults and
foreign keys are enforced.

## This ticket's contract

1. **`INTEGER PRIMARY KEY`** assigns `max + 1` when the value is `NULL` or
   omitted (`1` on an empty table). A **`TEXT PRIMARY KEY`** rejects
   `NULL`.
2. **`UNIQUE`** rejects a duplicate value (`ERR constraint`); several
   `NULL`s are fine. The primary key is unique too.
3. **`NOT NULL`** rejects `NULL`.
4. **`DEFAULT <literal>`**: a column omitted from an `INSERT`'s column
   list reads its default; an explicit `NULL` does not.
5. **`REFERENCES parent(column)`** is enforced (`PRAGMA foreign_keys=ON`
   semantics): inserting a child whose parent row does not exist, and
   deleting a parent a child still points at, are both `ERR constraint`
   (restrict) — and the failing statement changes nothing.
6. Constraints hold for `UPDATE` as well: a `SET` that would duplicate a
   unique value, null a required column or orphan a child is
   `ERR constraint`, with nothing applied.

## What the acceptance suite does

The cumulative suites still run (tickets 0-8), and the differential
generator now emits the constrained DDL (`PRIMARY KEY`, `UNIQUE`,
`NOT NULL`, `DEFAULT`, `REFERENCES`) and works against it: `NULL` and
omitted primary keys exercise auto-assignment, column-list inserts
exercise defaults, foreign-key columns read real parent values (or
`NULL`), and probes cover the violations (a `NULL` into `NOT NULL`, a
duplicate `UNIQUE` value, a child with no parent); deletes of referenced
parents must be refused. Every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
