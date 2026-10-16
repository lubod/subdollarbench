# Ticket 0 — Genesis: tables, insert, select

Build a **SQL database server** from scratch: raw TCP sockets, standard
library only, no embedded SQL engine and no SQL parser library. This is
the genesis ticket — every later ticket grows this same server.

The harness verifies you **differentially**: it generates a fresh random
schema and thousands of statements, sends them to your server and to a
reference SQLite, and every answer must match. Nothing can be
hard-coded, and the schema is different every run.

## Launch contract (fixed for the whole campaign)

- **Entrypoint**: an executable `./start.sh`, or a `Dockerfile`.
  `./start.sh` is started as `./start.sh --port <p> --data <dir>`, and the
  defaults must still work when nothing is passed. A `Dockerfile` receives
  those arguments **only when it declares an `ENTRYPOINT`**; a `CMD`-only
  image is started with no arguments, so read `SDB_PORT` / `SDB_DATA_DIR`
  (both are exported) or fall back to the defaults.
- **Port**: listen on **`0.0.0.0:5480`** by default; honor `--port <p>`.
- **Data directory**: `./data` by default; honor `--data <dir>`. The
  harness owns that directory and restarts the server on it between
  tickets, so anything you persist must survive.
- The server runs in a minimal Linux sandbox with python3, node, go and
  rust available. Foreground process, one listener.

## The text protocol (fixed for the whole campaign)

Requests are **one statement per line**, `\n`-terminated; a trailing `;`
is optional. Clients may **pipeline** (send several statements before
reading) and answers must come back in order.

```
OK <n>                    DML: rows affected. DDL: OK 0
ROWS <ncols>              then exactly one header line, the data lines,
<name>\t<name>...           then END <nrows>
<v>\t<v>...
END <nrows>
ERR <class> <text>        one line; the connection stays usable
```

- **Values**: INTEGER as decimal digits; `NULL` is `\N` (backslash, N);
  TEXT escapes `\` as `\\`, tab as `\t`, newline as `\n`; the empty string
  is an empty field.
- **Headers** are checked exactly: a bare column keeps its name
  (`SELECT id FROM t` -> header `id`).
- A statement that fails must not kill the connection or leave partial
  state: a multi-row `INSERT` with one bad row inserts **nothing**.

## The dialect (ticket 0 subset)

- **Types**: `INTEGER` (64-bit signed) and `TEXT`. No REAL, no BLOB, no
  dates. Tables are strictly typed: a clearly wrong value (e.g. the text
  `'zz'` into an INTEGER column) is `ERR type`.
- **Identifiers**: `[a-z_][a-z0-9_]*`, unquoted. Keywords are
  case-insensitive and the harness randomizes their case and the
  whitespace (spaces and tabs).
- **Literals**: decimal integers with unary minus; single-quoted strings,
  in which `''` escapes a quote and nothing else is special (a backslash
  is an ordinary character); `NULL`.

## This ticket's contract

1. `CREATE TABLE <name> (<col> INTEGER|TEXT, ...)` — 2-6 columns.
2. `INSERT INTO <name> VALUES (v, v, ...), (v, ...)` — one or more rows,
   all columns in order.
3. `SELECT * FROM <name>` and `SELECT <col>, <col> ... FROM <name>`.
4. Errors with the exact class token:
   - `no_table` — unknown table
   - `no_column` — unknown column
   - `exists` — `CREATE TABLE` on an existing name
   - `arity` — wrong number of values/columns
   - `syntax` — malformed statement (including incomplete input)
   - `type` — wrong-type value

## What the acceptance suite does

A seeded generator creates 3-5 tables with random names and sends
creates, multi-row inserts and selects; every answer is compared to the
reference (rows may arrive in any order unless the statement has
`ORDER BY`). Errors are compared by class token only, never by message
text. It also opens 8 concurrent clients that must all see each other's
committed rows, and sends garbage bytes, an unterminated line and a 2 MiB
line: none of them may kill the server, and a fresh connection must
still answer afterwards.

## Constraints

- **Standard library only.** No SQLite/DuckDB/Postgres bindings or
  binaries, no vendored engine sources, no SQL parser libraries — not in
  test or helper files either. The sandbox scanner checks every file in
  the workspace, and a violation disqualifies the ticket.
- **Reference**: a server speaking this exact protocol runs at host
  `ref-sql`, port 5480 (`printf 'SELECT 1\n' | nc ref-sql 5480`). Use it
  to check answers while you develop; it is unreachable during grading.
- Self-test before finishing, e.g.:

  ```bash
  printf 'CREATE TABLE t (id INTEGER, s TEXT)\nINSERT INTO t VALUES (1, '\''a'\''), (2, NULL)\nSELECT * FROM t\n' \
    | nc 127.0.0.1 5480
  ```

- `SPEC.md` in the workspace holds the whole contract so far (launch,
  protocol, dialect, constraints, every ticket to date). The harness
  rewrites it before each ticket.

- When done: create `/workspace/READY`.
