# SPEC.md — sdb-sql: a SQL database, verified differentially

Harness-owned: rewritten before every ticket, so edits here are discarded. Tickets that passed appear in full, oldest first; a later ticket extends the earlier ones and wins where they differ. A ticket marked FAILED or SKIPPED was not passed: its section is collapsed, it is **not graded and not required**, and no later contract rests on it. The last section is the ticket you are working on now.

---

<!-- ticket 0: Genesis: tables, insert, select (passed) -->

# Ticket 0 — Genesis: tables, insert, select

Build a **SQL database server** from scratch: raw TCP sockets, standard
library only, no embedded SQL engine and no SQL parser library. This is
the genesis ticket — every later ticket grows this same server.

The harness verifies you **differentially**: it generates a fresh random
schema and thousands of statements, sends them to your server and to a
reference SQLite, and every answer must match. Nothing can be
hard-coded, and the schema is different every run.

## Launch contract (fixed for the whole campaign)

- **Entrypoint**: an executable `./start.sh` at the repo root.
  `./start.sh` is started as `./start.sh --port <p> --data <dir>`, and the
  defaults must still work when nothing is passed. It must `exec` the
  binary the harness builds under `target/release/`. There is no
  custom-image entrypoint: it would bypass the rules below.
  `SDB_PORT` / `SDB_DATA_DIR` are exported too, as a fallback. A clean
  stop sends **SIGTERM to every process in the container** (not just
  PID 1).
- **Build**: the harness runs `cargo build --release --offline --locked`
  in the workspace before every start. The workspace root's `Cargo.toml`
  has **no dependencies** (`[dependencies]`, `[dev-dependencies]` and
  `[build-dependencies]` empty or absent; no `[patch]`), and `Cargo.lock`
  is kept in the workspace. A failed build is a ticket failure with the
  compiler's first error.
- **The language rule**: the process listening on the candidate port must
  be an ELF under `target/release/` built by rustc, and no `python3` or
  `node` process may run in the server's launch tree. A workspace that is
  not a Rust server fails with one `[LANG]` line.
- **Port**: listen on **`0.0.0.0:5480`** by default; honor `--port <p>`.
- **Data directory**: `./data` by default; honor `--data <dir>`. The
  harness owns that directory and restarts the server on it between
  tickets, so anything you persist must survive.
- The server is **Rust 1.100.0**, standard library only. `python3` and
  `node` exist for test scripts only; they may not serve. Foreground
  process, one listener.

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

## Performance (rules `.47`)

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

**Durable** means each acknowledged commit is on disk (`fsync` or
`fdatasync`) before the `OK`. The harness counts the disk's flush
requests during the 1-client W4 window; fewer flushes than commits
floors W4. If the reference's counter cannot be read, the check is a
harness fault (no score).

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

---

<!-- ticket 1: Expressions and WHERE (passed) -->

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

- `SPEC.md` in the workspace holds the whole contract so far (launch,
  protocol, dialect, constraints, every ticket to date). The harness
  rewrites it before each ticket. Answers can be checked against the
  reference server at `ref-sql:5480`.

- When done: create `/workspace/READY`.

---

<!-- ticket 2: Sorting and paging (passed) -->

# Ticket 2 — Sorting and paging

Ticket 1's contract is expressions and `WHERE`. This ticket adds **`ORDER BY`**,
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

- `SPEC.md` in the workspace holds the whole contract so far (launch,
  protocol, dialect, constraints, every ticket to date). The harness
  rewrites it before each ticket. Answers can be checked against the
  reference server at `ref-sql:5480`.

- When done: create `/workspace/READY`.

---

<!-- ticket 3: UPDATE and DELETE (passed) -->

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

---

<!-- ticket 4: Persistence (passed) -->

# Ticket 4 — Persistence

Everything your server has written so far lives only as long as the process
(tickets 0-3). This ticket makes it survive a restart. The harness stops the
server **cleanly** (`SIGTERM`), starts it again on the same directory, and
every row written before the stop must still answer exactly the same.

## Launch contract (unchanged, but now it matters)

- `./start.sh --port <p> --data <dir>`; defaults port `5480` and `./data`.
- **`--data` is your store.** The harness owns that directory: it hands you
  the *same* path across restarts and across tickets, and it may start you on
  a directory that a previous ticket already wrote. Keep all state there;
  never wipe it on startup, and never keep the only copy in memory or in a
  scratch directory the harness does not own.
- A clean stop is `SIGTERM` (delivered to every process in the
  container, so a non-`exec` `start.sh` still reaches the server); you
  have a few seconds to flush. (Ticket 5 adds
  `SIGKILL`, torn writes and double restarts.)

## What the acceptance suite does

The differential suite of the earlier tickets that passed still runs (cumulative), plus:

1. **Persistence**: the harness writes a seeded dataset, stops the server,
   starts it again on the same `--data`, and re-reads the data. Answers are
   compared byte for byte, so a format that loses rows, changes order or
   mangles values fails.
2. **Upgrade check (starts here)**: from this ticket on, the data your server
   writes is re-checked by every later ticket's verification. A build that
   cannot read its own earlier store is a regression of *this* ticket.

A failing verification comes with a minimal statement list: run it from an
empty database and compare your answers with the expected ones.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries (the
  scanner checks every file, tests included).
- `SPEC.md` in the workspace holds the whole contract so far.
- When done: create `/workspace/READY`.

---

<!-- ticket 5: Crash safety (passed) -->

# Ticket 5 — Crash safety

Persistence (ticket 4) survives a *clean* stop. This ticket survives a
**kill -9**: the harness may kill the process at any moment, without warning,
and after a restart everything the server acknowledged must still be there —
and nothing it did not acknowledge may be half-applied.

## What the acceptance suite does

The cumulative suite of the earlier tickets that passed still runs, plus:

1. **Acked rows survive a kill -9.** The harness sends single-row inserts,
   counts the acks, and kills the server right after one. After a restart,
   exactly the acknowledged rows must be present — every acked one, and no
   half-written extra.
2. **A big `UPDATE` killed mid-flight is all-or-nothing.** 50k rows are
   updated in one statement; the kill lands while it runs. After a restart
   the column is uniformly old or uniformly new — never a mixture, and no
   row may disappear.
3. **A torn tail recovers.** The last bytes of your newest data file may be
   missing (a small partial trailing record — a few hundred bytes cut from
   the region your last commit grew, as after a power cut). On startup you
   must recover every statement committed *before* the damaged tail; the
   single most recent statement is allowed to be lost, older ones are not.
   Refusing to start, or losing older rows, fails.
4. **Two restarts in a row change nothing.**
5. **A restart is serving again within 20 seconds** for a store the size
   this campaign produces (tens of thousands of rows).

## What "acked" means for you

Do not answer `OK` before the change is durable: a kill right after the ack
must not lose it. Write with fsync, keep a recoverable log or WAL-like
structure, validate records (length or checksum) while recovering, and treat
a trailing partial record as absent. `--data` is still the whole store.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `SPEC.md` in the workspace holds the whole contract so far.
- When done: create `/workspace/READY`.

---

<!-- ticket 6: Transactions (passed) -->

# Ticket 6 — Transactions

Crash safety (ticket 5) makes single statements durable. This ticket adds
**transactions**: several statements that take effect together or not at
all, across a connection that is killed mid-flight.

## This ticket's contract

1. **`BEGIN` / `COMMIT` / `ROLLBACK`**, one transaction per connection.
2. **Atomicity**: inside a transaction, other connections must not see your
   changes until `COMMIT`; a `ROLLBACK` (or a crash) discards them entirely.
   A `COMMIT` survives everything.
3. **Errors inside a transaction** do not abort it: the failing statement
   changes nothing and the transaction is still open (matching SQLite).
4. **`ERR txn`** for a nested `BEGIN` and for `COMMIT`/`ROLLBACK` with no
   transaction active.
5. Statements outside a transaction stay autocommit, as before.

## What the acceptance suite does

The cumulative suites of the earlier tickets that passed still run, and the differential
generator now writes whole transactions (a `BEGIN`, one or more DML
statements, then `COMMIT` or `ROLLBACK`) plus the `txn` error probes, so
your answers must match the reference's for every one. On top:

- **Isolation**: a second connection never sees an uncommitted row; after
  `COMMIT` it does; after `ROLLBACK` it never does.
- **Kill with a transaction open**: the committed rows survive the kill -9
  and restart; the uncommitted ones leave no trace.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.

---

<!-- ticket 7: Aggregates (passed) -->

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

## Performance this phase

The phase-end perf score adds **W3** (two-table join + `GROUP BY`, 50k x
5k rows); W1 and W4 still run. The full contract is in ticket 0's section
of `SPEC.md`.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.

---

<!-- ticket 8: Joins (passed) -->

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

---

<!-- ticket 9: Constraints (passed) -->

# Ticket 9 — Constraints

Ticket 8's contract is joins over many tables. This ticket makes the schema **mean
something**: primary keys, uniqueness, required columns, defaults and
foreign keys are enforced.

## This ticket's contract

1. **`INTEGER PRIMARY KEY`** assigns `max + 1` when the value is `NULL` or
   omitted (`1` on an empty table). A **`TEXT PRIMARY KEY`** rejects
   `NULL`.
2. **`UNIQUE`** rejects a duplicate value (`ERR constraint`); several
   `NULL`s are fine. The primary key is unique too.
3. **`NOT NULL`** rejects `NULL`.
4. **`DEFAULT <literal>`**: a column omitted from an `INSERT`'s column
   list reads its default; an explicit `NULL` does not.
5. **`REFERENCES parent(column)`** is enforced (`PRAGMA foreign_keys=ON`
   semantics): inserting a child whose parent row does not exist, and
   deleting a parent a child still points at, are both `ERR constraint`
   (restrict) — and the failing statement changes nothing.
6. Constraints hold for `UPDATE` as well: a `SET` that would duplicate a
   unique value, null a required column or orphan a child is
   `ERR constraint`, with nothing applied.

## What the acceptance suite does

The cumulative suites of the earlier tickets that passed still run, and the differential
generator now emits the constrained DDL (`PRIMARY KEY`, `UNIQUE`,
`NOT NULL`, `DEFAULT`, `REFERENCES`) and works against it: `NULL` and
omitted primary keys exercise auto-assignment, column-list inserts
exercise defaults, foreign-key columns read real parent values (or
`NULL`), and probes cover the violations (a `NULL` into `NOT NULL`, a
duplicate `UNIQUE` value, a child with no parent); deletes of referenced
parents must be refused. Every answer must match the reference's.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.

---

<!-- ticket 10: Subqueries (passed) -->

# Ticket 10 — Subqueries

Ticket 8's contract is joins: tables combined side by side. This ticket puts a whole
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

---

<!-- ticket 11 (current): Indexes + perf gate -->

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
