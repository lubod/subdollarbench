# Ticket 2 — Sorting and paging

Expressions and `WHERE` work (ticket 1). This ticket adds **`ORDER BY`**,
**`LIMIT`/`OFFSET`** and **`DISTINCT`**. Verification stays differential.

## This ticket's contract

1. **`ORDER BY`** names output columns or their aliases, several of them
   comma-separated, each `ASC` or `DESC` (default `ASC`). The suite
   always orders by **every** output column, so the expected order is
   total and is compared **exactly** (not as a multiset).
2. **`NULL` ordering**: `NULL` sorts first ascending and last
   descending.
3. **`LIMIT <n>` / `OFFSET <n>`**: applied after `ORDER BY`. The suite
   emits `LIMIT` only together with `ORDER BY`, with or without
   `OFFSET`.
4. **`DISTINCT`**: `SELECT DISTINCT ...` keeps one copy of each result
   row; `NULL`s compare as equal to each other.
5. A statement without `ORDER BY` has no guaranteed order: the suite
   compares those results as multisets, so any order is accepted.

## What the acceptance suite does

Same differential loop as before, plus ordered selects (each ordering by
every output column, with random `ASC`/`DESC`), `LIMIT`/`OFFSET` and
`DISTINCT`. Rows are compared in wire form: `NULL` is `\N`, text is
escaped, integers are plain decimal.

## Constraints

- Standard library only, no embedded engines or parser libraries.
- Self-test, e.g.:

  ```bash
  printf 'CREATE TABLE t (id INTEGER, s TEXT)\nINSERT INTO t (id, s) VALUES (3, '\''b'\''), (1, NULL), (2, '\''a'\'')\nSELECT id, s FROM t ORDER BY id\nSELECT DISTINCT s FROM t\nSELECT id FROM t ORDER BY id DESC LIMIT 2\n' \
    | nc 127.0.0.1 5480
  ```

- When done: create `/workspace/READY`.
