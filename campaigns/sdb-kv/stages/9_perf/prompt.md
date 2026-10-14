# Ticket 9 — Performance: p99 regressed under pipelined load

The dashboard and hardening shipped (tickets 7-8). Now scale: clients
have started **pipelining** — writing many commands back-to-back without
waiting for replies — and the p99 latency under that pattern is bad, and
raw throughput has not moved since the first ticket. Make the server
faster without breaking anything: the acceptance suite re-runs the whole
ladder, including durability and the crash tests.

## Deliverable

Performance work on the existing server, measured by the harness against
**your own ticket-0 measurement**.

## Contract additions

1. **Pipelining must work, in order.** A client may write N commands in
   one burst; the server must answer all of them, in the order received,
   correctly. (The acceptance suite sends 200 commands in a single write
   and checks every reply positionally.)
2. **Throughput gate: >= 1.3x your ticket-0 baseline.** The harness
   measured your genesis implementation's GET throughput (median of five
   100k-request runs, same method used at every ticket). This ticket
   requires the current server to reach **at least 1.3x that number**,
   measured identically. Typical paths: read-path optimizations, batching
   or reducing per-command overhead, a faster language/runtime, better
   concurrency. You do not need to know the exact baseline number — just
   make the server materially faster than your first version.
3. **Latency budget.** A single sequential `GET` must stay fast: the
   acceptance suite measures 300 sequential GETs and requires p99 <= 10ms
   (median well below). Do not trade correctness or latency for
   throughput.
4. **No regressions.** Everything earlier must stay green: durability
   (fsync-before-ack) still holds, the AOF crash test still passes, auth
   still works when `SDB_PASSWORD` is set, and the dashboard still serves.

## What the acceptance suite does

- Pipelining: 100 `SET` + 100 `GET` in one write; all 200 replies
  checked in order.
- Latency: 300 sequential GETs; p99 budget enforced.
- Throughput: the harness re-measures this stage with the same bench
  method it used at ticket 0 and requires >= 1.3x.

## Constraints

- Same protocol, same commands, same durability guarantees.
- Standard library only.
- Everything earlier must stay green.
