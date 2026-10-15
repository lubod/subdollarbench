# Ticket 0 — Front door: a reverse proxy that routes

Ops needs a front door for the fleet: a **reverse HTTP proxy** in front of
three upstream servers. Build it from scratch — raw sockets only, no web
frameworks, no nginx/haproxy (that would defeat the point). This is the
genesis ticket: everything later builds on this workspace.

## Launch contract (fixed for the whole campaign)

- **Entrypoint**: an executable `./start.sh` that launches your proxy, or a
  working `Dockerfile`. It must listen on **`0.0.0.0:8091`**.
- **Upstreams**: three upstream HTTP servers are reachable from your
  container at **`upstream1:80`**, **`upstream2:80`**, **`upstream3:80`**
  (docker network hosts). Each serves a distinct static body on `GET /`.
  They are always available during testing; dial them by hostname.
- Everything lives in the current workspace. Standard library + your
  language's runtime only.

## This ticket's contract

1. `GET /` on `:8091` returns **HTTP 200** with the body of one of the
   upstreams, **byte-exact** (status code and body preserved from the
   upstream).
2. Valid **HTTP/1.1** framing: `Content-Length` or chunked transfer
   encoding — never rely on connection-close to delimit the body.
3. Add **`X-Forwarded-For: <client-ip>`** (and, optionally,
   `X-Proxy-Latency`) to the request sent upstream.
4. Upstream failures must never crash the proxy; a request that cannot be
   served returns **502 Bad Gateway**.

## What the acceptance suite does

Fetches `GET /` through your proxy and asserts HTTP 200 with a body
matching one of the upstreams' markers, and that framing is
`HTTP/1.1` with a valid body delimiter.

## Constraints

- Raw sockets; no HTTP frameworks or prebuilt proxies.
- Do not consult nginx/haproxy/envoy sources — original code only.
- Self-test with `curl` against `upstream1/2/3` and your own port.
