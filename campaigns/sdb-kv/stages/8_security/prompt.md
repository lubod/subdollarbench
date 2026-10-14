# Ticket 8 — Security: one client can take us down

The dashboard shipped (ticket 7). Before we expose the box any further, a
security pass: a hostile client must not be able to crash the server,
starve other clients, or read the deployment secret. Keep everything
earlier green — the acceptance suite re-runs every earlier ticket first.

## Deliverable

Hardening for the existing server — RESP and dashboard — controlled by a
single environment variable.

## Contract additions

1. **`SDB_PASSWORD`.** When the variable is **set**, every RESP command
   except `AUTH` is refused with a `-NOAUTH ...` error until the client
   sends `AUTH <password>`; a wrong password gets an error reply, the
   right one gets `+OK`. When the variable is **unset**, the server must
   behave exactly as before (no auth) — every earlier ticket runs without
   it. When set, the dashboard requires HTTP Basic auth on every route
   (any username; the password as the password); without credentials it
   must return `401`.
2. **Argument cap, rejected before allocating.** A bulk string longer
   than 8MB, or a multibulk with more than 1024 arguments, must be
   answered with an error (or the connection closed) **without buffering
   it**. The acceptance suite runs your server with a 64MB memory cap: a
   frame you try to buffer will kill the process and fail the ticket.
3. **Fuzz survival.** Malformed frames — binary garbage, negative bulk
   lengths, absurd multibulk counts, truncated frames, replies sent where
   requests are due, oversized inline lines — must never crash or wedge
   the server. After each payload, a fresh client must be able to AUTH
   and PING.
4. **Slow-loris isolation.** Half-open connections (partial frames that
   never complete) must not block normal clients: with a dozen open, a
   fresh AUTH+PING must still complete within a couple of seconds.
5. **The secret must never appear in your logs.** No startup banner, no
   debug print, nothing on stdout/stderr.

## What the acceptance suite does

Restarts your server with `SDB_PASSWORD` set and a 64MB memory cap, then:
checks the RESP auth gate (no auth / wrong password / right password),
the dashboard Basic auth (401 without, 200 with), both oversized frames
(they must react under the cap), the fuzz corpus (liveness after each),
and the slow-loris probe. Finally it scans the container logs for the
password.

## Constraints

- Standard library only — no new dependencies.
- Everything earlier must stay green.
