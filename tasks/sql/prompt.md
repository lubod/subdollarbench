# sdb-sql — a differential SQL database

The benchmark's one remaining task: build a SQL **database server** from
scratch — raw TCP sockets, standard library only, no embedded SQL engine
and no SQL parser library — with its own on-disk format, answering a text
protocol that is checked **statement by statement against SQLite**.

This task runs as a **campaign**: `campaigns/sdb-sql/` holds the ticket
ladder (stages 0-20, tiers 1-7). Each ticket carries its own graded
prompt under `campaigns/sdb-sql/stages/<n>_<name>/prompt.md`; the ladder
is cumulative, and a later ticket's suite re-runs every stage the model
already passed. The full contract lives in
`docs/PROPOSAL-sdb-sql.md` (launch contract, text protocol, dialect per
ticket, ops tickets, replication, the PostgreSQL wire stage).

Tickets: 0 genesis · 1 expressions · 2 sorting/paging · 3 update/delete ·
4 persistence · 5 crash safety · 6 transactions · 7 aggregates · 8 joins ·
9 constraints · 10 subqueries · 11 indexes + perf gate · 12 concurrency
and isolation · 13 schema change · 14 observability · 15 tests and CI
gate · 16 larger than memory · 17 incident: disk full · 18 hostile input
and limits · 19 replication and failover · 20 PostgreSQL wire protocol.

Run one: `scripts/sql-cell.sh <model> [budget] [wall_min]`.
