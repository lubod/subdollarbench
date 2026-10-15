# Ticket 6 — Limits and hostile input

The front door is on the public internet now. Every byte a client sends is
attacker-controlled: header blocks of megabytes, declared bodies of
gigabytes, connections that dribble one byte and vanish. The proxy must
reject cheaply, stay bounded, and keep serving real clients.

## Contract additions

1. **Oversized request head**: a request whose request-line + headers
   exceed a sane cap (accept anything ≥ 8KiB as your limit; the suite
   sends 32KiB) must be answered with a **4xx** (431/413/400) or closed —
   and must not be buffered into memory.
2. **Oversized declared body**: a `Content-Length` larger than your
   request-body cap (accept anything ≥ 1MiB as your limit; the suite
   declares 100MB) must be rejected with **413** (or 4xx/close) **before**
   reading the body — never buffer it.
3. **Slow-loris protection**: a connection that sends a partial request
   head and then stalls must be **reclaimed within ~10 seconds** (close
   it, optionally a 408). Holding such sockets forever is a leak.
4. **Liveness**: normal requests must keep working while hostile ones are
   in progress, and after all of the above the proxy still serves 200s.
5. The harness runs your proxy under a **128MB memory limit** during this
   ticket: nothing may buffer unbounded client input.

## What the acceptance suite does

Restarts the proxy under the 128MB cap and, against the cumulative suite
plus a limits stage: 32KiB of headers (4xx or clean close), a declared
100MB body (4xx/close within seconds), a stalled half-request (reclaimed
within 12s) with normal traffic flowing meanwhile, and a final healthy
`GET /`.

## Constraints

- Standard library only; streaming, not buffering.
- Everything earlier must stay green.
