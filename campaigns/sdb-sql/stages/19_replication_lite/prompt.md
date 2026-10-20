# Ticket 19 (lite) — Replication with two nodes and PROMOTE

The shorter replication ticket: **two nodes**, no automatic election. The
harness starts node 1 as primary and node 2 as a replica
(`--replicaof ha1:<p1>`); the node list is `SDB_SQL_NODES=ha1:<p1>,ha2:<p2>`.

## This ticket's contract

1. **`SHOW ROLE`** answers `ROWS` with `name`/`value` rows: `role`
   (`primary` or `replica`) and `lsn`.
2. The replica serves reads and answers writes with `ERR readonly`.
3. **Sync commit**: a `COMMIT` is acknowledged only after the replica has
   it durably; the acked rows are on the replica before the failover.
4. **Manual failover**: the harness kills the primary and sends `PROMOTE`
   to the replica. It becomes primary with every acked commit and serves
   the cumulative SQL suite. (The survivor is alone; it may ack new writes
   locally.)
5. **No acked commit lost.**

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- When done: create `/workspace/READY`.
