# Ticket 7 — Aggregates

Tickets 0-6 are the core grammar. This ticket adds **aggregate
queries**: `COUNT`, `SUM`, `MIN`, `MAX`, with `GROUP BY` and `HAVING`.

## This ticket's contract

1. **`COUNT(*)`** counts rows; **`COUNT(x)`**, **`SUM(x)`**, **`MIN(x)`**,
   **`MAX(x)`** take one argument and **skip `NULL`s**.
2. **Empty input**: `COUNT` is `0`, `SUM` is `NULL`, `MIN`/`MAX` are
   `NULL`. A scalar aggregate (no `GROUP BY`) returns exactly **one row**,
   even when nothing matches.
3. **`GROUP BY`** builds one row per distinct value of the key
   expression; `NULL` is one group (not one per row). Group keys may be
   column expressions (`upper(c)`, `c + 3`, `length(c)`), and the select
   list holds aggregates and the group key — a bare non-grouped column is
   not tested.
4. **`HAVING`** filters groups after aggregation (the same three-valued
   comparison rules as `WHERE`; an aggregate in `WHERE` is `ERR syntax`,
   and so is an aggregate as a `GROUP BY` key).
5. `MIN`/`MAX` compare INTEGER and TEXT values with the same rules as
   `ORDER BY`. `SUM` gets INTEGER arguments; no `AVG` (no REAL).
6. Everything earlier still holds: no `ORDER BY` needed, but a query with
   `LIMIT` must be totally ordered.

## What the acceptance suite does

The cumulative suites of the earlier tickets that passed still run, and the differential
generator now writes aggregate queries — scalar aggregates, grouped ones
with a total `ORDER BY` on the key, and `HAVING` predicates over
`COUNT(*)`, `SUM`/`MIN`/`MAX` and the key — plus the aggregate error
probes (`GROUP BY zz_nocol`, `SUM(zz_gone)`, an aggregate in `WHERE`, an
aggregate as the `GROUP BY` key). Every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
