# Ticket 0 — SDB-KV MVP

We are building our own in-memory key-value server in this repository. This
is ticket 0: ship the minimum viable server. Later tickets will add
expiry, pub/sub, durability, dashboards, hardening, and clustering — so
build clean foundations, not a prototype you will have to throw away.

## Deliverable

A working server in `/workspace` that speaks RESP2 over TCP on port 6379.

- Entry point: `./start.sh` (or a `Dockerfile` that starts it). It must
  build if needed and then run in the foreground.
- One process, multiple concurrent clients, no external services.

## Required commands (stage 1 of the acceptance suite)

| Command | Reply |
|---|---|
| `PING` | `+PONG` |
| `ECHO <arg>` | bulk string reply |
| `SET k v` | `+OK` |
| `GET k` | bulk value, or `$-1` when missing |
| `DEL k` | `:1` / `:0` integer |
| `EXISTS k` | `:1` / `:0` |
| `INCR k` / `DECR k` | integer reply; non-integer value must be a RESP error |

Exact RESP2 framing matters: `+`, `-`, `:`, `$` (with length), `*`.
Errors use the `-ERR ...\r\n` shape.

Not in this ticket (later tickets will ask): TTL/expiry, pub/sub,
persistence, auth, HTTP.

## Constraints

- Standard library only. No key-value databases, no RESP client/server
  libraries, no prebuilt `redis-server`/`valkey`/`keydb` binaries or
  vendored engines. Raw TCP sockets.
- Name your binary neutrally (`server`, `sdb-kv`): NEVER `redis-server`,
  `valkey`, `keydb`, `nginx`, or similar — the compliance scanner
  disqualifies those names even for binaries you compiled yourself.
- Protocol parsing must be incremental: a command may arrive split across
  packets, and several commands may arrive in one packet. A malformed
  frame must not kill the server or leak the connection to other clients.

## Acceptance (run automatically)

1. RESP handshake: `PING`, `ECHO` with exact framing.
2. Key-value semantics: `SET`/`GET`/`DEL`/`EXISTS` including missing-key
   replies.
3. Counters: `INCR`/`DECR` including the error path for non-integer
   values.
4. Concurrent clients: at least two connections interleaved without
   cross-talk.
5. Malformed input: garbage and truncated frames must not take the
   server down (fresh connection answers `PING` afterwards).

## When done

Verify locally with `bash` (build + run + a few commands), then create
`/workspace/READY` to signal the framework for verification. If any
acceptance check fails, you will receive the diagnostics and may repair
in place — do not start over.
