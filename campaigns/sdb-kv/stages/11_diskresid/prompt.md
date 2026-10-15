# Ticket 11 — The dataset outgrew RAM (disk-resident store)

The store is durable, observable, tested, dashboarded, hardened, fast, and
HA-capable (tickets 0-10). Now it meets reality: **the dataset is much
larger than the memory the process gets**. Ops pins the container at
**192MB** and expects ~500MB of values to be served from
`SDB_DATA_DIR` on disk. Keep everything earlier green — the acceptance
suite re-runs the whole ladder first.

## Contract additions

1. **`SDB_DATA_DIR` holds the whole keyspace, not just the log.** When it
   is set, all persistent state lives there (you may keep a small index in
   memory — but the *values* may not all be resident: the process must
   serve data far exceeding its memory limit). When it is unset, earlier
   behavior is unchanged.
2. **Write path.** `SET` with values up to 240KiB must be acknowledged
   only after the data is durably on disk (the ticket-3 fsync-before-ack
   contract, now at scale). ~2000 large values are written through RESP
   while the container's 192MB limit is enforced. An implementation that
   holds every value in RAM is OOM-killed and fails.
3. **Read path.** Every acknowledged value must read back **byte-exact**
   (the acceptance suite spot-checks full 240KiB values), `EXISTS`/`DEL`
   behave as before, and expired keys still vanish.
4. **Crash recovery at scale.** The suite SIGKILLs the server and restarts
   it: every acknowledged value must still read back, and deleted keys
   must stay deleted.
5. Everything earlier stays green: RESP protocol, TTLs, pub/sub, INFO,
   dashboard, security, perf, HA modes.

## What the acceptance suite does

Restarts your server with `SDB_DATA_DIR` on a bind-mounted volume under a
**192MB memory cap**, writes ~2000 x 240KiB values (~480MB), spot-checks
40 full values byte-for-byte, deletes five keys and confirms they are
gone, then SIGKILLs and restarts the server and re-checks the same keys.

## Constraints

- Standard library only; raw sockets.
- No new flags required for recovery.
- Everything earlier must stay green.
