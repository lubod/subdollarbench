# Ticket 19 — Replication and failover

The server works alone; this ticket makes it survive losing a node. The
harness starts **three nodes**, one container each:

```
./start.sh --port <port> --data <dir>                 # node 1: primary
./start.sh --port <port> --data <dir> --replicaof ha1:<p1>   # nodes 2/3
```

The cluster's node list is exported as
`SDB_SQL_NODES=ha1:<p1>,ha2:<p2>,ha3:<p3>` (Docker aliases; the same value
is on every node).

## This ticket's contract

1. **`SHOW ROLE`** answers `ROWS` with `name`/`value` rows: `role`
   (`primary` or `replica`) and `lsn` (an integer that only grows).
2. **Replicas** serve reads and answer writes and DDL with `ERR readonly`.
3. **Sync commit**: a `COMMIT` is acknowledged only after at least one
   replica has it durably. A client that saw `OK` for a commit must find
   every row on the survivor after the primary dies. The harness freezes
   both replicas for about 1 s before a commit; `COMMIT` must not be
   acknowledged until one of them is resumed, and a 1 s freeze must not
   trigger an election.
4. **Failover**: kill the primary. Within **10 s** exactly one survivor is
   primary — the one with the highest LSN, ties broken by the lowest port —
   it holds every acked commit, and its writes reach the other survivor.
5. **Rejoin**: the old primary's container is recreated as a replica
   (`--replicaof <new primary>`, a fresh data directory); it rebuilds from
   the new primary's log, converges and refuses writes.
6. **After failover** the cumulative differential suite runs against the
   current primary.

The harness seeds a table, catches both replicas up, freezes them across
a commit, resumes one and waits for the commit, kills node 1, checks the
election and rows, writes to the new primary, brings node 1 back, and
finally runs the SQL suite against the winner.

## Constraints

- Standard library only; no embedded SQL engines or parser libraries.
- `--data` is the whole store; `SPEC.md` holds the whole contract so far.
- Replication is your protocol; only the observable contract above is
  graded.
- When done: create `/workspace/READY`.
