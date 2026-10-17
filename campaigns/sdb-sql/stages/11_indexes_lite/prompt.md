# Ticket 11 (lite) — Indexes, equality only

This shorter ticket adds **indexes** and gates **equality** lookups only;
range acceleration is not required.

## This ticket's contract

1. `CREATE [UNIQUE] INDEX name ON table (column)` and `DROP INDEX name`.
   An index is a schema object like a table: a name that is taken is
   `ERR exists`, an unknown table or column is `ERR no_table` /
   `ERR no_column`, dropping an unknown index is `ERR no_table`, and a
   `UNIQUE` index over already-duplicated values fails at creation with
   `ERR constraint` (registering nothing).
2. A `UNIQUE` index constrains `INSERT`/`UPDATE` exactly like a
   column-level `UNIQUE`: a duplicated non-`NULL` value is
   `ERR constraint` (several `NULL`s are fine).
3. No answer may change because of an index, and `CREATE INDEX` inside a
   transaction rolls back.
4. Indexes survive restart.

## The perf gate (equality only)

Two identical **200,000-row tables**, an index on one, **500** alternating
equality lookups: the indexed median must be **under 5 ms** and at least
**10x faster** than the unindexed median — measured again after a restart
of the server on the same store. Range predicates are **not** gated here.

## What the suite does

The cumulative suites you already passed still run, plus the index
grammar (`CREATE [UNIQUE] INDEX` / `DROP INDEX` interleaved with DML) and
the index probes (missing table, missing column, missing index on
`DROP`). Every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
