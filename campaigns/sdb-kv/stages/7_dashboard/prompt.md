# Ticket 7 — Ops wants a web dashboard

The store is durable, observable, and tested (tickets 3-6). Now ops wants
to actually *look* at it: a small web dashboard served next to RESP, on
the same box, with no internet access at all (the sandbox has no egress).
Keep everything earlier green — the acceptance suite re-runs every earlier
ticket first.

## Deliverable

An HTTP/1.1 server listening on port **8090** beside the RESP port 6379,
started by the same `./start.sh` (no extra flags, no second service to
start by hand).

## Contract additions

1. **`GET /`** returns `text/html` with a status page for the store.
   It must be self-contained: no external scripts, styles, fonts or CDNs.
   Absolute `http(s)` resource references (`src="http...`,
   `href="http...`) are treated as a failure — the dashboard must render
   with no network.
2. **`GET /api/stats`** returns `application/json` mirroring your INFO
   counters, with the same field names: `total_connections_received`,
   `total_commands_processed`, `keyspace_hits`, `keyspace_misses`,
   `expired_keys`, `rejected_connections`, `connected_clients`,
   `uptime_in_seconds`. Values may be numbers or numeric strings. Static
   or hardcoded values fail: the harness drives scripted RESP traffic and
   requires the counters to move by at least what it executed.
3. **`GET /events`** is a Server-Sent Events stream
   (`Content-Type: text/event-stream`). While a client holds the stream
   open, at least one `data:` frame containing JSON must arrive within
   2 seconds. Emit frames as commands/keys change, or on a sub-second
   timer — your choice.
4. **RESP must not degrade while the dashboard streams.** The harness
   keeps an SSE client open and runs RESP traffic concurrently; every
   command must still be answered.

## What the acceptance suite does

- Fetches `/` and checks HTML + the offline constraint.
- Reads `/api/stats` before and after randomized scripted traffic
  (hits, misses, an expiring key) and checks the deltas (same discipline
  as the INFO ticket).
- Opens `/events`, runs RESP traffic concurrently, and requires a JSON
  `data:` frame inside 2 seconds with all RESP commands answered.

## Constraints

- Standard library only — no web frameworks, no new dependencies. Raw
  sockets are expected.
- Ports: RESP stays on 6379; the dashboard listens on 8090.
- Everything earlier must stay green.
