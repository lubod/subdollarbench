# Ticket 2 — NACK: retries with delays

A worker that fails a job must be able to hand it back immediately — or
after a backoff — while every attempt is accounted for.

## Contract additions

1. **`NACK <id>`** -> `+OK`: the job becomes available **immediately**.
   The next lease reports `attempts` + 1.
2. **`NACK <id> <delay_ms>`** -> `+OK`: the job is not leasable until the
   delay passes, then it returns with `attempts` + 1.
3. NACK obeys the same ownership rules as ACK (`-ERR not-owner` from
   another connection, `-ERR not-leased` for unknown/expired ids).
4. Everything earlier stays green.

## What the acceptance suite does

Leases a job, nacks it, proves the immediate redelivery shows
`attempts` = 2; nacks it again with a 1.5s delay, proves it is invisible
until the delay passes and then returns with `attempts` = 3; acks it; and
verifies an unknown-id ack errors without crashing.
