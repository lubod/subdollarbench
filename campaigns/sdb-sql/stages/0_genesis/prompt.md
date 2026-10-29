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
  defaults must still work when nothing is passed. A `Dockerfile` image is
  started with **its own `CMD` followed by `--port <p> --data <dir>`**, so
  put the server in an exec-form `CMD` (or an `ENTRYPOINT` with no `CMD`)
  and the flags arrive as argv. `SDB_PORT` / `SDB_DATA_DIR` are exported
  too, as a fallback. A clean stop sends **SIGTERM to every process in the
  container** (not just PID 1), so a `start.sh` without `exec` still
  delivers it; `exec` remains the recommended form.
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

## Performance (rules `.43`)

Correctness is graded ticket by ticket. Speed is a **second axis** next to
points: points sort first, and a tie in points is broken by the perf
score. It is measured at the end of every phase on that phase's final
server; the perf run is harness time and never spends your budget or
counts toward the wall cap.

Your server and the SQLite reference run the same workload, alternating
rounds, each in a container with **2 CPUs and 2 GiB**. The reference is
`1.0`.

| ID | Workload | Size | Metric |
|---|---|---|---|
| W1 | point lookup by key, 1 client | 100k rows | p50 (p99 reported) |
| W2 | range + `ORDER BY ... LIMIT`, 1 client | 100k rows | p50 |
| W3 | two-table join + `GROUP BY` | 50k x 5k rows | p50 |
| W4 | durable autocommit single-row `INSERT`, 1 and 8 clients | 2 s | commits/s |
| W5 | mixed 80/20 read/write, 16 clients | 30 s | throughput (p99 reported) |

For each workload `r` is `reference / candidate` for latency and
`candidate / reference` for throughput, clipped to `[0.1, 100]`; W4
combines its 1-client and 8-client rates with a geometric mean. The perf
score is the geometric mean of the `r` values for the phase's workloads
(`1.0` = the reference, `2.0` = twice as fast). A wrong answer scores
that workload at the floor (`0.1`), so speed never buys a wrong result.

Phase 1 measures W1 and W4; phase 2 adds W3; phase 3 adds W2 and W5;
phase 4 runs all five. The sizes and the method are fixed.

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
