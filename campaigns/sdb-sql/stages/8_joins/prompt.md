# Ticket 8 — Joins

Ticket 7's contract is aggregates over one table. This ticket joins **two or three
tables** in one query.

## This ticket's contract

1. **`FROM a, b`** (comma) and **`a INNER JOIN b ON <pred>`** are inner
   joins: one output row per matching pair. A missing `ON` condition is a
   cross product.
2. **`a LEFT JOIN b ON <pred>`** (also `LEFT OUTER JOIN`) keeps every left
   row; when nothing matches, `b`'s columns are `NULL`.
3. **`ON`** takes a compound predicate over the columns joined so far
   (the same operator rules as `WHERE`).
4. **Qualified and bare names.** `a.x` (alias or table name) always
   resolves; a bare `x` resolves when exactly one source table has it,
   and is `ERR ambiguous` when two do. An alias (`a`) may be used to join
   a table to itself: `FROM t a, t b`.
5. **`SELECT *`** over a join lists every column of every table, in FROM
   order (duplicate names included).
6. `WHERE`, `ORDER BY` (over output columns or source columns),
   `DISTINCT` and `LIMIT` compose with joins; `UPDATE`/`DELETE` stay
   single-table.

## What the acceptance suite does

The cumulative suites of the earlier tickets that passed still run, and the differential
generator now writes join queries: comma joins, `INNER JOIN` and `LEFT
JOIN` (including 3-way) with `ON` link predicates, qualified references,
`SELECT *` over the join, and the join error probes (an ambiguous bare
name in a self-join, an unknown qualifier, a missing column in `ON`, an
unknown table). Every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
