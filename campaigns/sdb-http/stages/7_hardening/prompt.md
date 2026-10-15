# Ticket 7 — Hardening

The server is on a public network. Every request is adversarial: path
traversal, 32KiB header blocks, garbage request lines, connections that
stall mid-request. Reject cheaply, stay bounded, keep serving.

## Contract additions

1. **Path traversal**: `GET /files/../../etc/passwd` (raw, URL-encoded,
   and double-encoded forms) must never serve outside the store — answer
   `400`, `403` or `404` and never leak file contents.
2. **Oversized request head**: a request whose head exceeds your cap
   (accept anything >= 8KiB as the cap; the suite sends 32KiB) -> a 4xx
   (`431`/`400`/`413`) or a clean close — never unbounded buffering.
3. **Malformed request lines and unknown methods** -> a **4xx**
   (`400`/`405`/`501`) or a clean close, server survives.
4. **Slow-loris**: a connection that sends a partial request head and
   stalls must be **reclaimed within ~10 seconds** while other clients
   keep working.
5. Everything earlier stays green. The harness runs this ticket under the
   128MB cap as well.

## What the acceptance suite does

Cumulative suite plus a hardening stage: three traversal spellings
(400/403/404, no `root:` bytes), 32KiB of headers (4xx or close), a
malformed request line, a stalled partial head reclaimed within 12s with a
normal `GET /` succeeding meanwhile, and a final healthy request.
