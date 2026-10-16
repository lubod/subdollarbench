# Spec: sdb-sql — build a SQL database, one ticket at a time

Drafted 2026-10-15. Status: **spec, not built**. Replaces the four-campaign
portfolio (DECISIONS #11) and the single-shot tasks once landed.
Supersedes `docs/PROPOSAL-platform-campaign.md` (rejected: weak coupling
between subsystems, heavily memorized products).

## TL;DR

One campaign, one codebase, built from scratch: the model writes a SQL
database server in a language of its choice, stdlib only, and grows it
through **21 tickets in 7 tiers**: query core, storage and crash safety,
relational features, indexes and concurrency, ops, resilience,
replication and the PostgreSQL wire protocol.

- **Verification is differential.** The harness runs every generated
  statement against both the candidate and an in-process SQLite
  (`rusqlite`, bundled, already a dependency) and compares results. One
  seeded generator with a growing grammar profile covers most stages;
  scripted scenarios cover crashes, concurrency, performance and faults.
- **Coupling is architectural.** Every feature goes through the same
  parser, planner, executor and storage, so later tickets touch earlier
  code and regressions are real.
- **A failed stage gets one lite variant**, after a rollback to the last
  green commit. A failed lite marks the stage failed and the ladder
  **continues**; only stages whose `needs` are unmet are skipped.
- **Data survives upgrades.** From stage 4 on, each new build boots on the
  data directory the previous build wrote. Changing the file format means
  writing a migration.
- **Headline: difficulty-weighted points / campaign effective $.**

## Operator decisions (2026-10-15)

1. One combined campaign with rising difficulty: the product is a SQL
   database (option B).
2. Failure drops to a **lite variant only**; no stretch variants.
3. Retire the four campaigns (sdb-kv, sdb-edge, sdb-http, sdb-queue).
4. Retire the single-shot tasks (redis, http, pdf, wal, proxy, pubsub,
   binproto, queue).

Carried over: green-field genesis, model picks the language, stdlib only,
cumulative acceptance, regression = a check green earlier and red now,
fresh agent session per stage, git is the handoff, budget stop records
`reached` and is never a model failure, cost-first scoring, perf is a gate.

## Why a SQL database

| Property | Why it matters here |
|---|---|
| Free oracle | SQLite answers every generated query; no hand-written expected values |
| Seeded generation | fresh schema, data and queries per run: nothing can be hard-coded or memorized |
| Deep, natural ladder | parser -> executor -> storage -> WAL -> transactions -> joins -> indexes -> MVCC -> replication |
| Real coupling | indexes must keep joins correct; transactions meet the WAL; `ALTER` meets indexes and on-disk format |
| Natural lite variants | inner joins only, global lock instead of MVCC, manual promote: what a real team ships first |
| Stdlib-only is fair | a SQL engine needs sockets, files and threads, which every language's standard library has |

## Product contract (model-facing, shared by every stage)

### Launch

`./start.sh --port <p> --data <dir>` (or a `Dockerfile` whose entrypoint
accepts the same flags), foreground, one process. Defaults: port **5480**,
data `./data`. From stage 19: `[--replicaof <host:port>]`. From stage 20:
`--pg-port <p>` (default **5432**). The harness passes the flags with
worker offsets; the model never hard-codes ports or paths beyond the
defaults.

### Text wire protocol (port 5480)

Request: one statement per line, `\n`-terminated, trailing `;` optional.
Clients may pipeline: several lines before reading; responses come back in
order. Max line 1 MiB until stage 18 sets explicit limits.

Responses:

```
OK <n>                    DML: rows affected. DDL, BEGIN, COMMIT, ROLLBACK: OK 0
ROWS <ncols>              then exactly one header line, the data lines, END
<name>\t<name>...         header: output column names
<v>\t<v>...               one line per row
END <nrows>
ERR <class> <free text>   one line; the connection stays usable
```

Value encoding: INTEGER as decimal; TEXT with `\` -> `\\`, tab -> `\t`,
newline -> `\n`; NULL as `\N`; the empty string is an empty field.

Error classes (the harness compares the class, never the text):
`syntax`, `no_table`, `no_column`, `ambiguous`, `exists`, `arity`, `type`,
`constraint`, `txn`, `busy`, `limit`, `full`, `readonly`, `auth`.

### Dialect

The dialect is a strict subset of SQLite on STRICT tables. A
`SPEC.md` in the workspace (harness-owned, rewritten each stage) holds the
sections unlocked so far, so a fresh session can always read the rules.

- **Types:** `INTEGER` (64-bit signed) and `TEXT` (ASCII in tests). No
  REAL, BLOB or dates. Tables are strictly typed: a value of the wrong type
  is `ERR type`.
- **Identifiers:** `[a-z_][a-z0-9_]*`, unquoted. Keywords, function
  names and the `AS` of an alias are case-insensitive (the generator
  randomizes their case). Test identifiers are never SQL keywords; a
  candidate may reserve any word from the standard SQL keyword lists
  without failing a test.
- **Whitespace:** space, tab and runs of either separate tokens; a
  statement may end with an optional `;`. A tab inside a string literal is
  an ordinary character.
- **Literals:** decimal integers with unary minus (the generator
  parenthesizes negative literals inside expressions: `(-5)`); single-quoted
  strings in which `''` escapes a quote and **nothing else is special**
  (a backslash is an ordinary character: `'a\b'` has three characters);
  `NULL`. Test strings are ASCII, may be empty, may contain quotes, tabs,
  backslashes and spaces, and never begin with `END`.
- **Operator precedence** (tightest first, left associative within a
  level): unary `-`; `||`; `* / %`; `+ -`; `< <= > >=`; `= <> != IS
  IS NOT`; `NOT`; `AND`; `OR`. This is SQLite's table. The generator
  parenthesizes most nested operands but deliberately leaves some bare, so
  a parser that gets precedence or associativity wrong fails.
- **Arithmetic:** `+ - * / %`. Integer division truncates toward zero
  (`-7/2 = -3`); `%` takes the sign of the dividend (`-7%3 = -1`);
  division or modulo by zero yields `NULL`. Overflow is not tested.
- **Comparison and logic:** `= <> != < <= > >=`, `AND OR NOT`, `IS NULL`,
  `IS NOT NULL`, three-valued logic; results are `1`, `0` or `NULL`. TEXT
  compares bytewise. Comparing INTEGER with TEXT is not tested.
- **Functions:** `length`, `upper`, `lower`, `abs`, `coalesce`, and `||`
  (any `NULL` operand gives `NULL`).
- **Ordering:** `NULL` sorts first ascending and last descending.
  `DISTINCT` and `GROUP BY` treat `NULL`s as equal. `UNIQUE` allows any
  number of `NULL`s.
- **Statement atomicity:** a statement that fails changes nothing (a
  multi-row `INSERT` with one bad row inserts none). Tested outside an
  explicit transaction; inside one, a failed statement's earlier rows are
  not tested.
- **Not tested (any answer is accepted):** integer overflow, mixed-type
  comparison, inserting `'123'` into an INTEGER column or `5` into a TEXT
  column, non-ASCII text, bare non-grouped columns in aggregate queries, a
  scalar subquery returning more than one row.

### Constraints on the build

- Standard library only. **No embedded database engines of any kind**:
  no SQLite bindings (Python `sqlite3`, `node:sqlite`, `bun:sqlite`, Go
  or Rust drivers), no `sqlite3`/`duckdb`/`postgres` binaries, no vendored
  engine sources, no SQL parser libraries. The compliance scanner checks
  imports, linked libraries, vendored files and the candidate's process
  tree.
- Neutral binary name (`server`, `sdb-sql`), never a real database's name.

## The ladder

Lite weight is 0.4 of the stage's points. `--` means no lite: a fail is
final for that stage, but the ladder continues. Suites: **D** = differential
generator with the grammar profile of every passed stage; **S** = scripted
scenario.

| # | Tier | Ticket | New in the profile / scenario | Needs | Lite | Budget | Turns |
|---|---|---|---|---|---|---|---|
| 0 | 1 Core | Genesis: tables, insert, select | D: `CREATE TABLE`, multi-row `INSERT`, `SELECT *`/columns; S: concurrent clients, malformed input | -- | -- | $0.20 | 40 |
| 1 | 1 Core | Expressions and `WHERE` | D: expressions, `WHERE`, aliases, `SELECT` without `FROM`, column-list `INSERT` | 0 | -- | $0.12 | 30 |
| 2 | 1 Core | Sorting and paging | D: `ORDER BY`, `LIMIT`/`OFFSET`, `DISTINCT` | 1 | -- | $0.10 | 24 |
| 3 | 1 Core | `UPDATE` and `DELETE` | D: `UPDATE ... SET` (old-row semantics), `DELETE`, affected counts | 1 | -- | $0.10 | 24 |
| 4 | 2 Storage | Persistence | S: clean restart keeps everything; upgrade check begins | 0 | -- | $0.15 | 30 |
| 5 | 2 Storage | Crash safety | S: kill -9 after acks, mid-statement kill, torn tail, double restart | 4 | acked statements survive kill -9; no torn tail or mid-statement atomicity | $0.15 | 36 |
| 6 | 2 Storage | Transactions | D: `BEGIN`/`COMMIT`/`ROLLBACK`; S: crash inside an open transaction | 5 | -- | $0.15 | 30 |
| 7 | 3 Relational | Aggregates | D: `COUNT`/`SUM`/`MIN`/`MAX`, `GROUP BY`, `HAVING` | 1 | -- | $0.12 | 30 |
| 8 | 3 Relational | Joins | D: `INNER`/`LEFT JOIN`, comma joins, aliases, qualified columns, 3-way | 1 | inner joins only | $0.15 | 30 |
| 9 | 3 Relational | Constraints | D: `PRIMARY KEY` (+ auto-assign), `UNIQUE`, `NOT NULL`, `DEFAULT`, `REFERENCES` | 3 | -- | $0.12 | 30 |
| 10 | 3 Relational | Subqueries | D: scalar, `IN (subquery)`, `IN (list)`, `EXISTS`, correlated, `INSERT ... SELECT` | 7 | -- | $0.15 | 30 |
| 11 | 4 Engine | Indexes + perf gate | D: `CREATE [UNIQUE] INDEX`, `DROP INDEX`; S: lookup speed ratio | 3, 4 | equality lookups only | $0.15 | 36 |
| 12 | 4 Engine | Concurrency and isolation | S: lost-update, transfer invariant, readers vs open writer | 6 | one global lock | $0.20 | 40 |
| 13 | 4 Engine | Schema change | D: `ALTER TABLE ADD/DROP/RENAME COLUMN`, `RENAME TO`, `IF [NOT] EXISTS` | 4 | -- | $0.12 | 30 |
| 14 | 5 Ops | Observability | S: `EXPLAIN` plan shape, `SHOW STATS` exact counter deltas | 11 | -- | $0.12 | 24 |
| 15 | 5 Ops | Tests and CI gate | S: `./test.sh` green twice, coverage >= 80%, sensitivity reverts | 7, 8 | 60% coverage, sensitivity kept | $0.20 | 36 |
| 16 | 6 Resilience | Larger than memory | S: 64 MiB memory cap, data many times larger; scans, external sort, joins | 2, 4 | -- | $0.20 | 40 |
| 17 | 6 Resilience | Incident: disk full | S: tmpfs ENOSPC; reads serve, writes `ERR full`, no corruption, recovery | 5 | -- | $0.15 | 30 |
| 18 | 6 Resilience | Hostile input and limits | S: `AUTH`, fuzzing, nesting depth, statement size, timeouts, slow-loris, connection cap | 0 | -- | $0.15 | 30 |
| 19 | 7 Distributed | Replication and failover | S: 3 nodes, sync commit, automatic failover, rejoin | 6 | 2 nodes, manual `PROMOTE` | $0.20 | 40 |
| 20 | 7 Distributed | PostgreSQL wire protocol | S: `psql` + harness client, extended query protocol | 1 | simple query protocol only | $0.20 | 40 |

Full ladder: $3.20 and 680 turns before lite attempts; the campaign-wide
cap stays a real stop. Lite attempts use 60% of the stage's budget and
turns.

## Stage acceptance details

Each ticket prompt follows the existing style (story, deliverable,
failure modes exercised, constraints, "when done: create
`/workspace/READY`"). Hidden acceptance below.

**0. Genesis.** D with profile {create, insert, select}. Header names
checked exactly. Errors: `no_table`, `no_column`, `exists`, `arity`,
`syntax`, `type`.
S: 8 interleaved clients see each other's committed rows; garbage bytes,
an unterminated line and a 2 MiB line do not kill the server, and a fresh
connection still answers.

**1. Expressions and WHERE.** Type-directed expression generation (an
integer expression never meets a text comparison). `SELECT` without `FROM`.
Every non-bare output column is aliased by the generator, so header names
are always well defined.

**2. Sorting and paging.** When the generator emits `ORDER BY`, it orders
by every output column (random `ASC`/`DESC`), so the expected order is
total and compared exactly; `LIMIT`/`OFFSET` appear only with `ORDER BY`.
Without `ORDER BY`, results compare as multisets.

**3. UPDATE and DELETE.** `SET a=b, b=a` swaps (all right-hand sides read
the old row). `DELETE` without `WHERE` empties the table. `OK <n>` counts
are compared.

**4. Persistence.** Write a seeded dataset, `SIGTERM`, restart on the same
`--data`, re-run the read queries: identical. **Upgrade check starts here**
(see below).

**5. Crash safety.** (a) A client sends N autocommit `INSERT`s, collects
acks, SIGKILL at a random ack count; after restart every acked row exists
and no un-acked statement half-applied. (b) An `UPDATE` touching 50k rows
is killed mid-flight: after restart it is all or nothing. (c) The harness
kills the server, then cuts a few hundred bytes from the tail of the file
the last commit grew (a small partial trailing record — no real design can
recover from losing a whole flushed commit): startup recovers every
committed statement, the newest one excepted. (d) Two restarts in a row
change nothing. The ladder runs against a throwaway copy of the store so
the persistent one stays small.
Reuses the wal/AOF kill harness and ack accounting. Lite: acked rows
survive kill -9 only.

**6. Transactions.** D runs whole transactions per session, including
`ROLLBACK`, errors inside a transaction (the statement fails, the
transaction stays open, matching SQLite), and `ERR txn` for nested `BEGIN`
or a bare `COMMIT`. S: a second connection never sees uncommitted rows;
SIGKILL with an open transaction leaves none of it; a committed
transaction survives entirely.

**7. Aggregates.** `COUNT(*)`/`COUNT(x)`/`SUM(x)`/`MIN(x)`/`MAX(x)`,
`GROUP BY` (any single expression) and `HAVING`. `SUM` of no rows is
`NULL`, `COUNT` is `0`; aggregates skip `NULL`s; `GROUP BY` puts `NULL`s
in one group and `NULL` still sorts first. `SUM` takes an INTEGER
argument in tests (`SUM` over TEXT is not tested), and a query mixing an
aggregate with a bare, non-grouped column is not tested. No `AVG` (no
REAL). An aggregate in `WHERE` or as a `GROUP BY` key is `ERR syntax`.

**8. Joins.** Up to 3 items, comma or `INNER JOIN`, and `LEFT [OUTER]
JOIN` (unmatched left rows pad with `NULL`); `ON` with compound
predicates; a table alias (`FROM t a`) qualifies references and enables
self-joins; a bare name resolves when exactly one item has it and is
`ERR ambiguous` otherwise; `SELECT *` over a join lists all columns in
FROM order (duplicates included); `ORDER BY` may name an output column or
a source column of a `SELECT *`. Lite profile: comma and inner joins
only.

**9. Constraints.** `INTEGER PRIMARY KEY` assigns max+1 (1 when empty) on
`NULL` or omitted value; a `TEXT PRIMARY KEY` rejects `NULL`.
`REFERENCES t(col)` with restrict semantics: a child with a missing parent
and deleting a referenced parent are `ERR constraint`. The oracle runs
with `PRAGMA foreign_keys=ON`.

**10. Subqueries.** Scalar subqueries are always wrapped by the generator
in an aggregate, so they return exactly one row. Correlated `EXISTS` and
`IN`; `INSERT INTO t SELECT ...`.

**11. Indexes.** D: random `CREATE INDEX` / `DROP INDEX` interleaved with
DML; results must not change, and indexes survive restart. S (perf gate):
a 200k-row table; 500 point lookups on an indexed column vs the same
lookups on an unindexed copy: indexed p50 under 5 ms **and** at least 10x
faster than the scan. Range predicates (`<`, `BETWEEN`-style `AND`) gated
the same way in the full variant; lite gates equality only. Runs under
BenchLock.

**12. Concurrency and isolation.** Required outcome: serializable.
A conflicting transaction may get `ERR busy`; the harness retries it. No
operation may hang more than 5 s.
(a) Lost update: 16 clients x 50 `BEGIN; SELECT; UPDATE v=v+1; COMMIT`:
final value exactly 800. (b) Transfers: concurrent balance moves; the sum
is constant in every snapshot a reader takes. (c) Full only: with a write
transaction open on connection A, a `SELECT` on connection B returns the
pre-transaction value within 1 s (readers do not block on writers). Lite
(a global lock) passes (a) and (b), not (c).

**13. Schema change.** `ADD COLUMN` with and without `DEFAULT` (existing
rows read the default), `DROP COLUMN` (refused if indexed: `ERR
constraint`), `RENAME COLUMN`, `RENAME TO`, `IF NOT EXISTS`, `IF EXISTS`.
Every change survives restart and keeps indexes valid.

**14. Observability.** `EXPLAIN <select>` returns `ROWS` whose lines
contain `SCAN <table>` or `INDEX <table>.<index>`; with an index on an
equality predicate the plan must name it, without one it must scan.
`SHOW STATS` returns name/value rows; after scripted traffic the harness
checks exact deltas of `statements`, `selects`, `inserts`, `updates`,
`deletes`, `commits`, `rollbacks`, `errors`, `connections_current`,
`connections_total`.

**15. Tests and CI gate.** Same contract as sdb-kv stage 6: `./test.sh`
offline, green twice, under 120 s; coverage computed by the harness where
tooling exists (Go, Node, Python), self-reported otherwise. Sensitivity:
in a scratch copy, revert the stage 7 (aggregates) and stage 8 (joins)
diffs; `./test.sh` must go red. Persistent gate from here on.

**16. Larger than memory.** Candidate under a 64 MiB cgroup cap; harness
loads about 400 MiB of rows. Full-table aggregate, `ORDER BY` over the
whole table (external sort), and an indexed join must return correct
results without being OOM-killed. Reuses the sdb-kv stage 14 cgroup
plumbing.

**17. Disk full.** `--data` on a small tmpfs, filled by the harness mid
traffic: reads keep serving, writes return `ERR full` and never ack, no
data file is corrupt, and writes resume after space is freed without a
restart. Reuses the sdb-kv ENOSPC injector.

**18. Hostile input and limits.** With `SDB_SQL_PASSWORD` set, a
connection must send `AUTH <password>` first (`ERR auth` otherwise; the
password never appears in logs or errors). Grammar-mutation and byte
fuzzing never crash the server; expressions nested 10,000 deep give
`ERR limit`, not a stack overflow; statements over 16 MiB give `ERR
limit`; a statement running over 5 s is cancelled with `ERR limit`;
slow-loris clients do not starve others; beyond 256 connections new ones
are refused cleanly.

**19. Replication and failover.** Three nodes on ports 7101-7103 (+ offset),
separate data dirs, `--replicaof`. `SHOW ROLE` returns `role` and `lsn`.
Replicas serve reads and refuse writes (`ERR readonly`). A `COMMIT` is
acknowledged only after at least one replica has it durably. Kill the
primary: within 10 s exactly one survivor is primary (highest `lsn`, tie
to the lowest port), holds every acked commit, and accepts writes that
reach the other survivor. Restart the old primary: it rejoins as a replica
and converges. Lite: two nodes, `PROMOTE` on the replica, no acked commit
lost. The cumulative suite then runs against the current primary.

**20. PostgreSQL wire protocol.** Protocol 3.0 on `--pg-port`: SSL
request refused with `N`, startup, cleartext password when
`SDB_SQL_PASSWORD` is set, `ParameterStatus` (`server_version`,
`client_encoding=UTF8`), `ReadyForQuery` with `I`/`T`/`E`, simple query
(`RowDescription` with `int8`/`text` OIDs, `DataRow`, `CommandComplete`
tags such as `SELECT 3`, `INSERT 0 2`, `UPDATE 1`), `ErrorResponse` with
SQLSTATE mapped from the error classes (`42601` syntax, `42P01`
no_table, `42703` no_column, `23505`/`23502`/`23503` constraints, `40001`
busy, ...). Full adds the extended protocol (Parse/Bind/Describe/Execute/
Sync with `$1` parameters), which real drivers use. Graded with `psql`
and a harness client. Only the wire protocol is PostgreSQL; the dialect
stays the one above. The text protocol on 5480 must keep working.

## Cross-stage rules

**Cumulative suite.** The suite at stage k is the union of the passed
stages' suites, at the variant that passed: a lite pass contributes the
lite profile (for example, inner joins only), never the full one. Failed
and skipped stages contribute nothing.

**Upgrade check (data migration).** The harness owns a data directory
outside the workspace that lives for the whole campaign. After a passing
stage k >= 4, the harness writes a seeded **sentinel dataset k** and
records the oracle's answers to its queries. At stage k+1, before the
regular suite, the new build boots on that directory and must answer every
prior sentinel's queries correctly. A failure is a regression of class
`upgrade`. The ticket text says plainly: "the data written by your
previous release is handed to the new build." A rollback restores the
data directory snapshot along with the code.

**Fresh seeds.** Each verification (including the re-verify after a repair
round) uses a new seed: new table and column names, data and queries.
A repair cannot overfit to the reported example.

## Variant mechanics

1. **Full attempt:** initial turns plus one repair round with feedback.
2. **On fail:** the failed attempt is tagged `sN-full-fail`; the workspace
   and the data directory roll back to the last passing stage.
3. **Lite attempt:** fresh session, the lite ticket, a note that the
   previous attempt was rolled back, and the minimal failing repro from
   the full attempt (see Q3).
4. **On lite fail:** roll back again, mark the stage failed, continue.
5. **Skipping:** a stage whose `needs` include a failed stage is
   `skipped` and costs nothing. A lite pass satisfies `needs`; each lite
   must still provide the interface later stages rely on.

## Verifier design (`src/verifier/sql/`)

| Module | Role |
|---|---|
| `schema` | seeded schema: 3-5 tables, 2-6 columns, INTEGER/TEXT, random names |
| `generator` | type-directed grammar generator; a **profile** of feature flags (one per stage, plus lite subsets); bounded depth; randomized keyword case and whitespace |
| `oracle` | `rusqlite` in-memory reference; translates DDL (appends `STRICT`, `NOT NULL` on primary keys, `PRAGMA foreign_keys=ON`); maps SQLite errors to classes; **discards** any statement whose reference result is REAL or touches a "not tested" rule, and regenerates |
| `wire` | text-protocol client: framing, escaping, pipelining |
| `compare` | ordered vs multiset results, headers, `OK` counts, error class |
| `shrink` | delta-debugs a failing statement sequence to a minimal repro for the repair feedback |
| `scenarios` | crash, upgrade, isolation, perf, memory, ENOSPC, hostile, replication, PG wire |

Self-tests that gate the verifier itself (no Docker, no spend):
- **Oracle vs oracle:** the full generator against two independent
  SQLite connections (memory and file) agrees 100% over 100k statements.
- **Mutant toy engines:** a small in-test engine with planted bugs (wrong
  NULL ordering, no old-row `UPDATE` semantics, broken `LEFT JOIN`, index
  not maintained on `DELETE`) must be caught by the matching profile.
- **Flake bar:** each stage passes 20/20 against the reference adapter.

**Reference adapter (live proof).** A small Python server backed by its
own `sqlite3` (allowed for the reference only) speaking the text
protocol, used to prove the harness end to end for stages 0-18. It proves
the verifier, not solvability; solvability comes from a strong-model
calibration run. Stages 19-20 need their own reference work.

## Scoring

- Stage points = tier number x (1.0 full | 0.4 lite | 0 failed or
  skipped). The maximum is 76 points: T1 4, T2 6, T3 12, T4 12, T5 10,
  T6 18, T7 14.
- **Headline: points / campaign effective $** (API + compute floor,
  unchanged formula).
- Reported, not scored: per-stage profile (full / lite / failed /
  skipped), cost curve, turns, regressions, upgrade failures, `reached`.
- Perf stays a gate; no throughput multiplier.

## Retirement

Doctrine kept: archive and annotate; delete only with a reason (DNS
precedent, DECISIONS #2).

- **Campaigns** sdb-kv, sdb-edge, sdb-http, sdb-queue: `campaign` refuses
  new runs; eval-only replay of archived workspaces still works. Specs,
  grid logs and boards stay as history, labeled retired in the portal.
- **Single-shot tasks** redis, http, pdf, wal, proxy, pubsub, binproto,
  queue: `run`, `matrix`, `enqueue` and the UI stop offering them. `eval`,
  `replay`, `leaderboard` and the portal keep reading history.
  Batch-anchored throughput denominators retire with redis/http.
- **Verifier code** for the retired tasks becomes dormant. Reused
  directly by sdb-sql: the kill/restart and ack-accounting harness, the
  cgroup memory cap, the ENOSPC injector, slow-loris and multi-node launch
  plumbing, compliance scanning, BenchLock. Deleting dormant verifiers is a
  separate call (Q6); the 80% coverage gate holds either way.

## Harness changes

| Area | Change |
|---|---|
| Campaign spec | stage + `tier`, `needs`, `lite {prompt, profile, budget_usd, max_turns}` |
| Orchestrator | rollback, lite attempt, continue past failures, `needs` skipping, points rollup (`src/campaign.rs`, pure logic, unit-tested) |
| Task type | new `sql` task, `SqlVerifier`, acceptance as `sql_stages` |
| Data lifecycle | campaign-owned data dir, snapshot/restore on rollback, sentinel datasets, upgrade check |
| Launch | `--port --data [--replicaof] [--pg-port]` contract (from the sdb-kv HA launcher) |
| Workspace | harness writes `SPEC.md` (unlocked dialect sections) each stage |
| Compliance | embedded-engine rules: imports, linked libs, vendored sources, process tree |
| Image | `postgresql-client` (`psql`) in the harness image for stage 20 |
| Rules | new RULES stamp; sdb-sql is its own board, never pooled |

Not needed, compared with the platform proposal: a multi-suite executor.
sdb-sql is one task type on one port family.

## Resolved questions (operator, 2026-10-15)

- **Q1. Tier weights: tier number (1..7).** Recalibrate only if the first
  grid shows a tier mispriced.
- **Q2. Reference during development: yes.** The agent gets `ref-sql`
  (the SQLite-backed adapter) on its run network to probe exact output
  formats, like `ref-redis` today. It must be unreachable from the
  candidate at verification time; a harness test pins that.
- **Q3. Repair feedback: the minimal shrunk repro.** Fresh seeds on
  re-verify stop overfitting to it.
- **Q4. Lite weight: 0.4** (recommended, pending operator confirmation).
  Below 0.5, two lite passes can never outscore one full pass in the
  same tier; the failed full attempt's cost already penalizes lite via
  points/$, so the weight need not be harsher.
- **Q5. PostgreSQL finale: keep stage 20.** `postgresql-client` joins the
  harness image.
- **Q6. Dormant verifiers: keep now, delete in phases** (recommended,
  pending operator confirmation). The fault-injection plumbing sdb-sql
  reuses lives inside the old task paths today. After stages 0-18 land
  and that plumbing is extracted into shared modules, the old verifiers
  are deleted the DNS way: data archived, DECISIONS entry, `eval`/`replay`
  of retired tasks then read-only.

## Review resolutions (2026-10-15, pre-build)

Findings from the pre-build review, resolved here; the build follows these.

1. **Reference isolation is now repo-wide.** The live harness had
   `ref-redis`/`ref-http` attached to the run network for the whole trial,
   so a candidate could proxy to a reference and fake stages. Fixed:
   `SandboxManager::detach_references()` / `reattach_references()`, called
   around every `run_verification` (the agent and the bench keep the
   refs), with a live test pinning resolve → unreachable → resolve. sdb-sql
   inherits the same mechanism; Q2's "unreachable at verification" is
   required for the new task and already true for the old boards.
2. **Stage 15 coverage gate softened**: coverage gates only where the
   harness computes it (Go, Node, Python); for other languages the
   **sensitivity revert is the only gate** (self-reported coverage is
   gameable and is reported, not scored).
3. **Stage 14 `SHOW STATS` semantics pinned**: counters count
   statement-bearing request lines only; the harness's own probes,
   keepalive reconnects and pipeline reads are excluded. Exact deltas are
   computed over a scripted window that starts and ends with a barrier
   statement.
4. **Stage 18's 5s cancel is deterministic**: the harness sends a pinned
   monster query (seeded self-join over the large table) whose result can
   never complete under the cap, so cancellation is the only passing
   outcome (no host-speed dependence).
5. **Oracle hardening**: an explicit discard table (any REAL result —
   including `SUM` integer→REAL promotion and integer overflow — plus
   every "not tested" rule) with regeneration; a **minimum comparable-check
   floor per stage** (a stage that discards too much fails the verifier
   self-test, not the candidate); a **profile-coverage assertion** in the
   generator self-tests (every feature of a profile appears at least N
   times per 10k statements) so no stage can silently test nothing.
6. **Error-class mapping pinned**: SQLite messages → classes in one table
   in the oracle module, unit-tested (including ALTER/DROP-specific
   messages and `datatype mismatch` → `type`); unknown messages fail the
   verifier self-test loudly rather than being classified by guess.
7. **`reached` is defined** under continue-past-failures: the highest
   stage **passed** (lite or full); the highest attempted stage is
   reported alongside, as are per-stage profile and regressions.
8. **Ops guards**: an optional campaign wall-clock cap (the ladder is
   ~680 turns plus lite attempts; a full run is multi-hour) and a disk
   budget for the data dir (400 MiB datasets, ENOSPC tmpfs, sentinel
   snapshots) with cleanup on rollback and at campaign end.
9. **`psql` comes from a pinned client container** on the run network
   (the repo's `bench-client` pattern), not from host packages; the image
   digest is pinned in the launch code.
10. **Rollup/portal shape first**: the rollup gains per-stage `profile`
    (full/lite/failed/skipped), `points`, `tier`, `needs`, and
    `upgrade` failures; sdb-sql is its own board (never pooled). This
    lands before the first grid.
11. **Prompts after a rollback say so**: every stage whose workspace was
    rolled back (post-failure continue, or a lite attempt) opens with
    "your stage-N work was rolled back because …".
12. **Rollback restores everything the harness owns**: code (last green
    commit), the data-dir snapshot, and harness-owned workspace files
    (`SPEC.md`).
13. **Q4 (lite weight 0.4) and Q6 (phased retirement)** are confirmed as
    recommended, per the operator's "fix, document, build" directive.

## Rollout

1. **Orchestrator:** `tier`, `needs`, `lite`, rollback, continue,
   scoring, rollup. Unit tests only.
2. **Verifier core:** schema, generator (profiles 0-3), oracle, compare,
   shrink, wire client; oracle-vs-oracle and mutant-engine self-tests.
3. **Reference adapter** and live proof for stages 0-3 through the
   pipeline.
4. **Storage tier:** stages 4-6, data lifecycle, upgrade check, crash
   scenarios.
5. **Stages 7-13**, then **14-18**, then **19-20**, each with a live proof
   and the 20/20 flake bar.
6. **Retirement switch** for old tasks and campaigns; portal labels;
   README rewrite.
7. **Calibration:** one strong model end to end; re-scope any stage no
   model can pass.
8. **Grid** on the new stamp.
