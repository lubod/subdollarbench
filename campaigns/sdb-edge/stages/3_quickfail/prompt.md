# Ticket 3 — Fail fast under hangs

Ticket 2 survived upstreams that **refused** connections. Production
taught us the nastier case: an upstream whose process is **frozen** — the
kernel still accepts TCP connections, nothing ever answers. A proxy
without upstream timeouts parks its clients on that black hole.

## Contract additions

1. **Per-request budget under a hang**: while one upstream is frozen
   (connections accepted, no bytes returned), every client request must
   still complete with **200 in under 2.5 seconds**. This means: bound the
   time you wait on an upstream (connect + response), abandon it on
   timeout, and **retry another** upstream for the same client request.
2. Requests already in flight to a hung upstream must be retried, not
   surfaced as 502/504 (as long as a healthy upstream can serve).
3. Recovery: after the upstream thaws it rejoins rotation within 10s.
4. Everything from tickets 0–2 stays green (round-robin, keep-alive,
   failover under refused connections).

## What the acceptance suite does

The cumulative suite plus a quick-failover stage: the harness freezes one
upstream, fires 10 sequential requests and checks that **each** returns
200 and that the **worst** took under 2500ms, then thaws it and requires
rejoin within 10s.

## Constraints

- Standard library only.
- Do not paper over this by removing the upstream permanently: it must
  rejoin when healthy (ticket 2's rejoin check still runs).
