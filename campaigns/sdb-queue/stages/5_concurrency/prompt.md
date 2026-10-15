# Ticket 5 — Many consumers, no duplicates

The fleet runs dozens of workers on one queue. Under contention the broker
must never hand the same job to two workers.

## Contract additions

1. **No duplicate delivery**: leasing is atomic — concurrent `LEASE` calls
   from many connections each get distinct jobs (or `+NONE`).
2. **No loss under contention**: every enqueued job is eventually handed
   out exactly once; acked jobs never reappear.
3. Lease operations must not serialize the whole broker into a single
   global stall: other connections can still `ENQ`/`ACK` while leases
   churn.
4. Everything earlier stays green.

## What the acceptance suite does

Enqueues 200 jobs, runs 6 concurrent consumers that lease (10s leases) and
ack in a loop until the queue drains, and asserts: zero ids were handed to
two consumers, and all 200 jobs were acked.
