# Ticket 6 — Counters that add up

Operators need to see the queue's state without touching jobs.

## Contract additions

1. **`STATS <queue>`** -> `+STATS enqueued=<n> acked=<n> nacked=<n>
   inflight=<n> available=<n> dead=<n>` (that order, all six fields).
2. The numbers are **per queue** and must add up under any workload:
   - `enqueued` is everything ever enqueued to this queue;
   - `acked` counts completed jobs; `nacked` counts failed deliveries
     (including retries);
   - `inflight` is currently leased; `available` is waiting;
   - `dead` counts jobs that moved to `<queue>.dead`.
3. Everything earlier stays green.

## What the acceptance suite does

Builds a known workload on one queue — 10 enqueues, 4 ack cycles, 2 nack
cycles, 1 lease left in flight — then reads `STATS` and expects exactly
`enqueued=10 acked=4 nacked=2 inflight=1 available=5 dead=0`.
