# Ticket 1 — Visibility leases and ownership

Workers crash. A job handed out must come back when the worker never
answers, and it must not be handed to two workers at once.

## Contract additions

1. **Invisibility**: while a job is leased, other `LEASE` calls on the same
   queue get `+NONE` (or a different job) — never the leased job.
2. **Expiry**: if the lease is not acked/nacked within `lease_ms`, the job
   becomes available again. The next delivery reports `attempts` = 2 (each
   delivery increments the count).
3. **Ownership**: a lease belongs to the connection that took it. `ACK`
   from any other connection answers `-ERR not-owner`; acking an unknown or
   expired id answers `-ERR not-leased`.
4. Everything earlier stays green (FIFO, fidelity, `+NONE`, `PING`).

## What the acceptance suite does

Leases a job, proves a second connection cannot lease it while the lease is
live, waits out the lease, leases it again (same id, `attempts` = 2),
then proves the original connection can no longer ack it and the current
owner can.
