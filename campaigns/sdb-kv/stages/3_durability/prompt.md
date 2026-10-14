# Ticket 3 — Durability

Pub/sub shipped (ticket 2). Ops has one hard requirement before we can go
anywhere near production: **an acknowledged write must never be lost**,
even if the process is killed with `kill -9` a millisecond later. Keep
everything earlier green — the acceptance suite re-runs tickets 0-2.

## Deliverable

Write-ahead persistence for the existing server, same port, same protocol.

- All mutating commands (`SET`, `DEL`, TTL changes via any command,
  `INCR`/`DECR`) are recorded in an append-only file in the workspace.
- A reply of `+OK` (or an integer) for a mutation means the record is
  already durable on disk: fsync (or equivalent) BEFORE acknowledging.
- On startup, replay the log and restore the exact keyspace, including
  remaining TTLs.
- The log path must be stable across restarts (pick one, e.g.
  `./appendonly.log`), and the server must not require any flag to
  recover.

## Failure modes the acceptance suite exercises

1. **Kill -9 during traffic.** A client writes N keys and receives N acks;
   the server is SIGKILLed; the server is restarted; every acked key must
   be present with the right value.
2. **Torn tail.** The last record in the file may be truncated or
   garbage (crash mid-write). Startup must tolerate that: recover every
   complete record, discard the partial tail, and start serving.
3. **Replay is idempotent.** Restarting twice in a row must not duplicate
   or corrupt state.

## Performance note

fsync-per-mutation is acceptable; you do not need group commit. The
scored throughput of this benchmark is measured on the read path, so do
not trade correctness for write speed.

## Constraints (unchanged)

Standard library only; raw TCP sockets; neutrally named binary; tickets
0-2 semantics unchanged. Pub/sub delivery may be non-durable (live
only) — persist only the keyspace.

## When done

Test with `bash`: write keys, `kill -9` the process, restart, verify the
keys and a TTL survive; then truncate the last byte of the log, restart
again, verify clean recovery. Keep tickets 0-2 green. Then create
`/workspace/READY`.
