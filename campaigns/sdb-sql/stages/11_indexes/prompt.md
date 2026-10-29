# Ticket 11 — Indexes and the perf gate

Ticket 9's contract is constraints that make the schema enforce invariants. This ticket
adds **indexes**: named physical structures that must change *speed*
without changing a single answer.

## This ticket's contract

1. **`CREATE [UNIQUE] INDEX name ON table (column)`** and
   **`DROP INDEX name`**. An index is a schema object like a table:
   - a name that is taken is `ERR exists`;
   - an unknown table is `ERR no_table`, an unknown column is
     `ERR no_column`, and dropping an unknown index is `ERR no_table`;
   - a `UNIQUE` index over already-duplicated values fails at creation
     with `ERR constraint`, and registers nothing.
2. **A `UNIQUE` index constrains later writes exactly like a
   column-level `UNIQUE`**: an `INSERT` or `UPDATE` that would duplicate
   a non-`NULL` value is `ERR constraint` (several `NULL`s are fine).
3. **No answer may change because of an index.** Every `SELECT` result
   is what it was without the index, and the statement-atomicity rule
   covers indexes too (`CREATE INDEX` inside a transaction rolls back).
4. **Indexes survive restart**: after the server restarts on the same
   store, an index is still there — still enforcing (2) and still fast.

## The perf gate

The verifier fills two identical **200,000-row tables**, creates an index
on one, and alternates **500 point lookups**
(`SELECT v FROM t WHERE k = ?`) between them:

- the indexed median must be **under 5 ms**, and
- at least **10x faster** than the unindexed median.

Range predicates (`WHERE k >= a AND k < b`, `ORDER BY k`) are gated the
same way. The gate runs under the bench lock once every stage passed, and
restarts the server in the middle: the index must still meet both bounds
afterwards (4).

## What the acceptance suite does

The cumulative suites still run, and the differential generator now
interleaves `CREATE [UNIQUE] INDEX` / `DROP INDEX` with DML; probes cover
an index on a missing table, an index on a missing column, and dropping a
missing index. Every answer must match the reference's, and the perf gate
above runs once all stages pass.

## Performance this phase

The phase-end perf score adds **W2** (range + `ORDER BY ... LIMIT`) and
**W5** (mixed 80/20 read/write, 16 clients, 30 s); W1, W3 and W4 still
run. The index gate above is separate and stays pass/fail. The full
contract is in ticket 0's section of `SPEC.md`.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
