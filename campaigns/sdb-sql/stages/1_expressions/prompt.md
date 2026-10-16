# Ticket 1 — Expressions and WHERE

Your SQL server works for tables, inserts and simple selects (ticket 0).
This ticket adds **expressions**, `WHERE`, `AS` aliases, `SELECT` without
`FROM`, and column-list inserts. Verification stays differential: fresh
seeded schema, thousands of statements, answers compared with SQLite.

## This ticket's contract

1. **Column-list INSERT**: `INSERT INTO t (col, col, ...) VALUES (...)` —
   names in any order, a subset of the columns; omitted columns are
   `NULL`.
2. **Expressions** in select items, `WHERE`, and (later) `UPDATE`:
   - arithmetic `+ - * / %` on INTEGERs: division truncates toward zero
     (`-7/2 = -3`), `%` takes the sign of the dividend (`-7%3 = -1`),
     division or modulo by zero yields `NULL`;
   - comparisons `= <> != < <= > >=`; `IS NULL` / `IS NOT NULL`;
   - `AND`, `OR`, `NOT` with three-valued logic: a `NULL` operand gives
     `NULL` unless the result is determined (`NULL AND false` is false,
     `NULL OR true` is true); results are `1`, `0` or `NULL`;
   - `||` concatenation: any `NULL` operand gives `NULL`;
   - functions `length`, `upper`, `lower`, `abs`, `coalesce(a, b)`.
3. **Aliases**: `SELECT <expr> AS <name>`; header names are checked
   exactly. Every non-bare output column in the suite is aliased.
4. **`WHERE`**: filters rows before output; a `NULL` predicate keeps
   nothing.
5. **`SELECT` without `FROM`**: `SELECT <expr> AS e0, <expr> AS e1` over
   constants.
6. **Operator precedence** (tightest first, left associative within a
   level): unary `-`; `||`; `* / %`; `+ -`; `< <= > >=`; `= <> != IS IS
   NOT`; `NOT`; `AND`; `OR`. The generator parenthesizes most nested
   operands but **deliberately leaves some bare**, so getting precedence
   or associativity wrong fails.

## Notes

- Integer expressions in the suite never overflow and never mix types:
  an integer expression is never compared with text. TEXT compares
  bytewise.
- Numeric-looking text is never inserted into an INTEGER column by the
  suite (use your judgement for that case; it is not graded).
- A bare column keeps its name as the header; an expression without an
  alias is not produced by the suite.

## What the acceptance suite does

Same differential loop as ticket 0, plus expression/`WHERE` statements.
Predicates are anchored on values that were really inserted, so filters
normally return rows; a server that ignores `WHERE` (or evaluates the
wrong tree) is caught. Update-style statements are not part of this
ticket.

## Constraints

- Standard library only, no embedded engines or parser libraries.
- Self-test, e.g.:

  ```bash
  printf 'CREATE TABLE t (id INTEGER, s TEXT)\nINSERT INTO t (id, s) VALUES (1, '\''a'\''), (2, NULL)\nSELECT id * 2 AS twice, upper(s) AS u FROM t WHERE id >= 1 ORDER BY id\n' \
    | nc 127.0.0.1 5480
  ```

  (`ORDER BY` arrives officially in ticket 2; if you already support it,
  the example works as shown.)

- When done: create `/workspace/READY`.
