# Ticket 8 (lite) — Joins, inner only

Ticket 7's contract is aggregates over one table. This shorter ticket joins **two or
three tables** with the *inner* forms; `LEFT JOIN` is not required.

## This ticket's contract

1. **`FROM a, b`** (comma) and **`a INNER JOIN b ON <pred>`** are inner
   joins: one output row per matching pair. A missing `ON` condition is a
   cross product.
2. **`ON`** takes a compound predicate over the columns joined so far
   (the same operator rules as `WHERE`).
3. **Qualified and bare names.** `a.x` (alias or table name) always
   resolves; a bare `x` resolves when exactly one source table has it,
   and is `ERR ambiguous` when two do. An alias (`a`) may be used to join
   a table to itself: `FROM t a, t b`.
4. **`SELECT *`** over a join lists every column of every table, in FROM
   order (duplicate names included).
5. `WHERE`, `ORDER BY`, `DISTINCT` and `LIMIT` compose with inner joins;
   `UPDATE`/`DELETE` stay single-table.
6. `LEFT JOIN` is **not** tested here (the full ticket asks for it).

## What the suite does

The cumulative suites of the earlier tickets that passed still run, with the join grammar
narrowed to comma and `INNER JOIN` shapes — no `LEFT JOIN` — plus the join
error probes (an ambiguous bare name in a self-join, an unknown qualifier,
a missing column in `ON`, an unknown table).

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
