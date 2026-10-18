# Ticket 3 — UPDATE and DELETE

Ticket 2's contract is sorting and paging. This ticket adds **`UPDATE`** and
**`DELETE`**, including the affected-count contract. Verification stays
differential.

## This ticket's contract

1. **`UPDATE <t> SET <col> = <expr> [, <col> = <expr> ...] [WHERE ...]`**
   - every right-hand side reads the **old row**: `SET a = b, b = a`
     swaps the two values;
   - the expression language is the one from ticket 1.
2. **`DELETE FROM <t> [WHERE ...]`**; `DELETE` without `WHERE` empties
   the table.
3. **Affected counts** (`OK <n>`) are compared exactly:
   - `UPDATE`: the number of rows **matched by `WHERE`** — even if the
     values did not change;
   - `DELETE`: the number of rows deleted;
   - `CREATE TABLE` and other DDL: `OK 0` (never the previous DML's
     count).
4. A failed statement changes nothing (atomicity), and the connection
   stays usable.

## What the acceptance suite does

Same differential loop, plus updates and deletes interleaved with
inserts and selects, so the data your server holds must keep matching
the reference. It includes the swap update (`SET a = b, b = a`) and
zero-row updates (`WHERE` matches nothing -> `OK 0`).

## Constraints

- Standard library only, no embedded engines or parser libraries.
- Self-test, e.g.:

  ```bash
  printf 'CREATE TABLE t (a INTEGER, b INTEGER)\nINSERT INTO t (a, b) VALUES (1, 2)\nUPDATE t SET a = b, b = a WHERE a = 1\nSELECT * FROM t\nDELETE FROM t\nSELECT * FROM t\n' \
    | nc 127.0.0.1 5480
  ```

  The expected answers: `OK 1` for the insert, `OK 1` for the update
  (one row matched), `ROWS 2` / `a\tb` / `2\t1` / `END 1`, `OK 1`,
  `ROWS 2` / `a\tb` / `END 0`.

- `SPEC.md` in the workspace holds the whole contract so far (launch,
  protocol, dialect, constraints, every ticket to date). The harness
  rewrites it before each ticket. Answers can be checked against the
  reference server at `ref-sql:5480`.

- When done: create `/workspace/READY`.
