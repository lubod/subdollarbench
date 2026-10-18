# Ticket 10 — Subqueries

Joins (ticket 8) combine tables side by side. This ticket puts a whole
**query inside an expression**: subqueries.

## This ticket's contract

1. **Scalar subqueries**: `(SELECT ...)` in a select list or comparison.
   In tests it is always wrapped in an aggregate, so exactly one row; a
   subquery with **more than one column** is `ERR arity`, and one over no
   rows is `NULL`.
2. **`IN (subquery)`** and **`IN (list)`**, plus the negated forms. Three
   valued: a `NULL` on the left is `NULL`; no match with a `NULL` in the
   set is `NULL`, not false; an **empty** set is false even for a `NULL`
   left side.
3. **`EXISTS (subquery)`** / `NOT EXISTS`: true iff the subquery yields
   rows. Correlated subqueries may reference the enclosing query's
   columns; an unqualified name resolves innermost first.
4. **Correlated scalar and `IN` subqueries** are evaluated per outer row.
5. **`INSERT INTO t SELECT ...`**: the select's rows are inserted with
   the same per-row constraint checks as `VALUES`.
6. Errors inside a subquery are reported even when no row would evaluate
   it (a missing table/column is checked when the statement is prepared).

## What the acceptance suite does

The cumulative suites of the earlier tickets that passed still run, and the differential
generator now writes subqueries: scalar subqueries as select items and in
comparisons (result types matched), `[NOT] IN (subquery)`, `IN (list)`
with anchored values and NULLs, correlated `[NOT] EXISTS`, and
`INSERT ... SELECT` whose select list matches the target's types. Probes
cover a missing table/column inside a subquery and a two-column scalar
subquery. Every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
