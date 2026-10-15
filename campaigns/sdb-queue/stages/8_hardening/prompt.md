# Ticket 8 — Limits and hostile input

The broker is on a shared network now. Every byte a client sends is
adversarial: 32 KiB payloads, garbage lines, connections that stall
mid-command. Reject cheaply, stay bounded, keep serving real workers.

## Contract additions

1. **Oversized payloads**: a payload beyond your cap (accept anything
   >= 8 KiB as the cap; the suite sends 32 KiB) must answer `-ERR ...`
   (or close the connection) — never be buffered into memory.
2. **Malformed input**: garbage lines, missing arguments, unknown commands
   answer `-ERR ...`; the connection stays usable and the broker survives.
3. **Slow-loris**: a connection that sends a partial command and stalls
   must be **reclaimed within ~10 seconds** (close it, optionally after an
   error line). Holding such sockets forever is a leak.
4. **Liveness**: normal workers keep working while hostile clients are
   active, and after all of the above the broker still serves 200-style
   traffic. The harness runs your broker under a **128MB memory limit**
   during this ticket.
5. Everything earlier stays green.

## What the acceptance suite does

Restarts the broker under the 128MB cap and, against the cumulative suite
plus a hardening stage: a 32 KiB payload (rejected), five malformed lines
(errors, connection still usable), a stalled partial command (reclaimed
within 12s) with a `PING` answered meanwhile, and a final healthy
enqueue/lease/ack cycle.
