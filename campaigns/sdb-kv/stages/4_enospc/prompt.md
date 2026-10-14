# Ticket 4 — Incident: the log volume filled up

Durability shipped (ticket 3). Production just paged us: during a write
burst the **log volume hit 100% and the server stopped taking writes**.
Nobody could store anything until an operator cleared space, and we have
no idea whether the log survived. This is the incident ticket: make the
server survive a full volume without losing data or lying to clients.
Everything earlier must stay green.

## Deliverable

Graceful behavior when the persistent log cannot be written, plus a
configurable data directory.

## Contract additions

1. **`SDB_DATA_DIR`**: when set, ALL persistent state (the AOF and any
   temp files that belong with it) lives in that directory. When unset,
   keep your current behavior (the workspace). The production volume is a
   small dedicated mount — do not hardcode the workspace path.
2. **Writes that cannot be persisted are errors, never successes.**
   If the log write (or its fsync) fails, reply with a RESP error
   (`-ERR ...`). It is a correctness violation to reply `+OK` for a
   mutation that is not durable, and it is a violation to apply the
   mutation in memory when it was not persisted (a subsequent `GET` of
   that key must not return the value).
3. **Reads keep serving.** With the volume full: `GET`/`EXISTS`/`TTL` on
   existing keys keep working, and `PING` keeps answering. The server must
   not crash, abort, loop forever, or wedge other connections.
4. **No corruption of what was already durable.** Keys acknowledged before
   the volume filled must still read back correctly while it is full.

## What the acceptance suite does

It restarts your server with `SDB_DATA_DIR` pointing at a small empty
volume (8MB), writes until persistence fails, then checks the four
behaviors above. A server that keeps acknowledging writes it cannot store
fails; a server that dies or hangs fails; a server that keeps reads
serving and reports errors honestly passes.

(Recovery-after-freeing-space and replay-after-incident are NOT checked in
this round — the harness cannot shrink/grow that volume mid-run.)

## Constraints (unchanged)

Standard library only; raw TCP sockets; neutrally named binary; tickets
0-3 semantics unchanged, including expiry and AOF crash recovery.

## When done

Test with `bash`: run the server with `SDB_DATA_DIR` on a small tmpfs
(`mkdir -p /tmp/small && mount -t tmpfs -o size=8m tmpfs /tmp/small` if
permitted, else use a large-value write loop against a small `ulimit -f`
or simply reason through the code path), write until errors appear,
confirm reads still work, then create `/workspace/READY`.
