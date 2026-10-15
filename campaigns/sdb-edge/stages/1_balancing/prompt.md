# Ticket 1 — Balance and persist

The front door works (ticket 0). One upstream is a single point of failure
and one client connection per request is wasteful. Make the proxy share
load and keep connections alive.

## Contract additions

1. **Round-robin balancing**: consecutive `GET /` requests rotate across
   **all three** upstreams. The acceptance suite sends 30 requests and
   requires every upstream to have served at least 5.
2. **Client keep-alive**: multiple sequential requests on **one** client
   TCP connection must all be answered (`Connection: keep-alive`,
   `HTTP/1.1` framing, no spurious closes between replies).
3. **Upstream connection reuse**: pool persistent connections to the
   upstreams instead of dialing a new socket per request. A new upstream
   connection per client request will not be caught by the black-box suite
   today, but it is part of the contract (and later latency gates).
4. Valid HTTP/1.1 framing on every response, as in ticket 0.

## What the acceptance suite does

The full cumulative suite: 30 requests must cover all three upstreams at
least 5 times each; 3 sequential requests on a single connection must all
return 200; framing must stay valid; routing (ticket 0) must remain green.

## Constraints

- Standard library only; no frameworks.
- Everything earlier must stay green — the suite re-runs it first.
