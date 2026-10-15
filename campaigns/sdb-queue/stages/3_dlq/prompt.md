# Ticket 3 — Dead letters after max attempts

A poison job that fails forever must stop cycling and land somewhere an
operator can inspect it.

## Contract additions

1. **Max attempts = 3**: when a job with `attempts` == 3 is nacked (or its
   lease expires), it moves to the **dead-letter queue `<queue>.dead`**
   instead of becoming available again.
2. A dead job is **never** redelivered on the live queue.
3. Dead-letter jobs live in `<queue>.dead` and can be leased like any
   other queue (`+JOB <id> <attempts> <payload>`), keeping their failed
   attempt count — reading the dead-letter queue is an **inspection**: it
   does not increment `attempts`.
4. Everything earlier stays green.

## What the acceptance suite does

Enqueues a job, leases/nacks it three times (attempts 1, 2, 3), proves the
live queue no longer serves it, finds it in `<queue>.dead` with
`attempts` = 3 and the same payload, acks it, and verifies new enqueues
keep working with increasing ids.
