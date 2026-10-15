# Ticket 0 — Front door: enqueue, lease, ack

Ops runs batch work and needs a **job queue broker** of its own: a small
TCP service that holds jobs, hands them to workers exactly once at a time,
and takes them back when workers die. Build it from scratch — raw sockets
only, no queues, brokers, or job libraries. This is the genesis ticket:
every later ticket builds on this workspace.

## Launch contract (fixed for the whole campaign)

- **Entrypoint**: an executable `./start.sh` or a working `Dockerfile`,
  listening on **`0.0.0.0:6385`**.
- **Persistence directory**: keep ALL state under **`./data`** (relative to
  the workspace). The harness restarts your broker in the same workspace
  after killing it: on startup you must replay `./data` and recover.
- **Wire protocol**: LF-terminated lines on TCP. Queue names and payloads
  use `[A-Za-z0-9._-]`; payloads are at most **8 KiB**; unknown/malformed
  commands answer `-ERR <reason>` and never crash the broker.
- One broker, many connections: each connection may hold several leases.

## This ticket's contract

1. **`ENQ <queue> <payload>`** -> `+OK <id>` — appends a job. Ids are
   monotonically increasing integers (per broker, starting at 0/1 — just
   never reuse or go backwards).
2. **`LEASE <queue> <lease_ms>`** -> `+JOB <id> <attempts> <payload>` or
   `+NONE` — hands the oldest waiting job to the caller. The first
   delivery reports `attempts` = 1.
3. **`ACK <id>`** -> `+OK` — the job is done and never comes back.
4. **FIFO**: jobs of one queue are leased in enqueue order while they are
   all available.
5. **Fidelity**: the payload comes back byte-exact.

## What the acceptance suite does

Enqueues three jobs, checks the ids increase and the payloads come back
FIFO with `attempts` = 1, acks each, then verifies the queue is empty
(`+NONE`). It also checks `PING` -> `+PONG`.

## Constraints

- Raw sockets; no queue/broker/job libraries (see the anti-cheat rules).
- Do not consult RabbitMQ/Kafka/NATS/Redis sources — original code only.
- Self-test with `nc`/`python3` before finishing:
  `printf 'ENQ q hello\nLEASE q 5000\n' | nc 127.0.0.1 6385`
