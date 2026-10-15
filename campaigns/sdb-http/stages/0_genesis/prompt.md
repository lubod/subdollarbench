# Ticket 0 — Front door: root and 404

Build an **HTTP/1.1 file server** from scratch: raw TCP sockets, no
frameworks (no express/flask/gin/axum wrappers, no nginx/caddy). This is
the genesis ticket — every later ticket builds on this workspace.

## Launch contract (fixed for the whole campaign)

- **Entrypoint**: an executable `./start.sh` or a working `Dockerfile`,
  listening on **`0.0.0.0:8080`**.
- **Server header**: identify yourself as `sdb-http` (the harness does not
  grade it, but ops does).
- **Store directory**: files live under `./data/files` (created on
  demand). The harness restarts the server in the same workspace between
  tickets, so the store must survive.
- Keep-alive connections, `Content-Length` framing, and `Connection`
  handling must be correct.

## This ticket's contract

1. **`GET /`** -> `200 OK` with a small, stable body (any text).
2. **Unknown paths** -> `404 Not Found` with a short body; the connection
   stays framed and reusable (keep-alive).
3. Malformed request lines answer `400 Bad Request` (never a crash).

## What the acceptance suite does

`GET /` must be 200 (twice on one connection: keep-alive), an unknown path
must be 404, and a malformed request line must not kill the server.

## Constraints

- Raw sockets only; no HTTP frameworks or prebuilt servers.
- Self-test with `curl -v http://localhost:8080/` before finishing.
