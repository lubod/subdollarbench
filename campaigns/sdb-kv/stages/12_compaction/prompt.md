# Ticket 12 — Disk keeps filling: compaction

The store is disk-resident now (ticket 11). Production has been
overwriting and deleting keys for weeks and the data directory has grown
to **20x the live data size** — the append-only history never shrinks.
Reclaim the space without losing anything.

## Contract additions

1. **The footprint must fall.** After heavy churn — keys overwritten many
   times and most of them deleted — the total bytes under
   `SDB_DATA_DIR` must fall to **at most 3x the live data size** within
   **30 seconds** after the churn stops. The acceptance suite measures the
   directory itself (bind-mounted), not any number your server reports.
   Concrete shape: 600 keys x 240KiB written six times (~860MB of records),
   then 400 of them deleted (48MiB live) — the footprint must reach
   <= 144MiB.
2. **How you reclaim is your choice.** A background thread, an idle
   trigger, a garbage-ratio threshold, a rewrite on startup — as long as
   it happens within the window and stays memory-bounded (the container
   keeps its 192MB cap).
3. **Correctness is untouched.** Every surviving key still reads back
   byte-exact after the compaction; deleted keys stay deleted; a SIGKILL
   + restart in the middle of the lifecycle still recovers every
   acknowledged value and nothing else.
4. Everything earlier must stay green.

## What the acceptance suite does

Starts your server with `SDB_DATA_DIR` bind-mounted under the 192MB cap,
writes 600 x 240KiB values six times over, deletes 400 of them, then
polls the data directory until the footprint falls below 3x live bytes
(30s budget) and spot-checks the survivors byte-for-byte. It then
SIGKILLs and restarts the server and re-checks survivors and deletions.

## Constraints

- Standard library only.
- Compaction must not break durability, in-flight writes, or the
  earlier tickets' guarantees.
