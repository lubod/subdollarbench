# Ticket 4 — Survive kill -9

A job queue that forgets acknowledged work is worse than no queue. Every
acknowledged operation must survive a hard kill, and in-flight work must
come back.

## Contract additions

1. **fsync before ack**: `ENQ`, `ACK`, `NACK` (and dead-letter moves) must
   be durable before the broker answers `+OK`. A `kill -9` immediately
   after the reply must not lose them.
2. **Restart semantics** (state lives in `./data`):
   - acked jobs never return;
   - jobs that were waiting stay waiting (with `attempts` unchanged: the
     next lease reports 1);
   - jobs that were **in flight** (leased, unacked) return to the queue
     with their attempt count preserved (an in-flight job with
     `attempts` = 1 reports `attempts` = 2 on the next lease);
   - dead-letter entries stay dead, with their attempts.
3. **Torn writes**: a crash mid-append must not corrupt earlier records.
4. Everything earlier stays green.

## What the acceptance suite does

Prepares a mixed state (one acked job, one waiting, one in flight with
attempts=1, one dead-letter job), the harness sends `kill -9`, restarts
your broker in the same workspace, and then verifies: the acked job never
returns, exactly the waiting + in-flight jobs come back (attempts
preserved), and the dead-letter entry is intact.
