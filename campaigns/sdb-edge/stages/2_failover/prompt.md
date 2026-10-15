# Ticket 2 — Survive upstream loss

A machine went down in production and the front door returned 502s while
it was unreachable through a deploy. That must never happen again: losing
an upstream must be invisible to clients.

## Contract additions

1. **Failover**: while one upstream is down, every client request must
   still be answered **200** from a healthy upstream — zero failed
   requests, none routed into the dead one.
2. **Rejoin**: when the upstream recovers, it must be taken back into
   rotation within **10 seconds** and serve traffic again.
3. The client must never see a 502 because *one* upstream is unhealthy;
   502 remains for the case where **no** upstream can serve.

## What the acceptance suite does

The cumulative suite plus a failover stage: the harness freezes one
upstream container (its process stops answering; TCP connects may still be
accepted), fires a burst of 20 requests through your proxy and requires
every one to return 200, then thaws the upstream and requires it to
rejoin rotation within 10s (observed through your proxy).

## Constraints

- No frameworks; original code.
- Everything earlier must stay green.
