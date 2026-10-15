You are building a **durable job queue broker** (SQS/NATS-JetStream-lite) from scratch.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional software engineering standards:
1. **Clean Architecture & Modularity**: Separate TCP networking, protocol parsing, queue/lease state, dead-letter handling, and crash recovery.
2. **Durability First**: Every acknowledged write must survive `kill -9` — explicit fsync/fdatasync (or flush + OS sync primitives) before the acknowledge path.
3. **Resilient Error Handling**: Socket EOFs, disconnects, malformed inputs, partial lines, and torn writes must never crash the broker or lose acknowledged state.
4. **Idiomatic Style & Best Practices**: Clean, idiomatic code for your chosen language.
5. **Production Readiness**: Production-grade, maintainable, performant, self-contained.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, curling, scraping, cloning, reading, or consulting the source code of any external queue/broker implementation (e.g. RabbitMQ, Kafka, NATS, Redis, nsq, Celery, bullmq, Sidekiq sources).
- **Allowed Resources**: Official protocol documentation and your own knowledge of leasing/visibility-timeout queue design.
- **Originality**: All code must be your own original implementation. Any access to external broker source code invalidates the benchmark run.

### Programming Language Freedom & Packaging (Dockerfile or start.sh)
- **Choose ANY Programming Language Freely** (Go, Rust, Python, Node.js, C, C++, etc.).
- **Pre-installed Toolchains**: Rust (cargo), Go, Python 3, Node.js, C/C++ in PATH.
- All code and configuration must reside in the current workspace.
- **Entrypoint (either)**:
  1. **Working Dockerfile (Recommended)**: builds your broker, exposes port 6385.
  2. **Executable `./start.sh`**: launches your broker on `0.0.0.0:6385`.
- **Persistence directory**: store ALL state under `./data`. On startup you MUST replay `./data` and recover; the harness restarts your broker in the same workspace after killing it.

### Wire Protocol (TCP port 6385, LF-terminated lines)
Queue names and payloads use `[A-Za-z0-9._-]`; payloads are at most 8 KiB; malformed commands answer `-ERR <reason>` and never crash the broker.

1. **ENQ**: `ENQ <queue> <payload>` -> `+OK <id>` (fsync before ack; ids strictly increasing).
   Delayed form: `ENQ <queue> <payload> IN <delay_ms>` -> `+OK <id>`.
2. **LEASE**: `LEASE <queue> <lease_ms>` -> `+JOB <id> <attempts> <payload>` or `+NONE`.
   The oldest waiting job; first delivery `attempts` = 1. A lease is owned by the connection that took it, is invisible to others, and expires after `lease_ms` (the job returns with `attempts` incremented).
3. **ACK**: `ACK <id>` -> `+OK` (done, never returns). Wrong connection -> `-ERR not-owner`; unknown/expired -> `-ERR not-leased`.
4. **NACK**: `NACK <id> [delay_ms]` -> `+OK` (return for redelivery, immediately or after the delay; attempts + 1 on the next lease).
5. **Dead letters**: after 3 failed attempts (nack or lease expiry at `attempts` = 3) the job moves to `<queue>.dead` and is never redelivered on the live queue. Leasing from `.dead` is an inspection: it does not increment `attempts`.
6. **REDRIVE**: `REDRIVE <queue> <id>` -> `+OK` moves a dead-letter job back with `attempts` reset.
7. **STATS**: `STATS <queue>` -> `+STATS enqueued=<n> acked=<n> nacked=<n> inflight=<n> available=<n> dead=<n>` (per queue; reads do not change them).
8. **PING** -> `+PONG`.

### Crash recovery
On startup scan `./data`, discard any torn trailing record, rebuild queues, in-flight leases (leased-but-unacked jobs return to the queue with attempts preserved), and dead letters. Acked jobs never return.

### Verification Criteria
1. Byte-exact protocol tests: ENQ/LEASE/ACK FIFO round trip, lease invisibility + expiry + ownership, NACK (immediate/delayed), dead-lettering at 3 attempts, STATS counters, delayed ENQ, REDLAIM/REDRIVE.
2. Crash test: the harness builds a mixed state, `kill -9`s your broker, restarts it, and verifies no acked loss and correct in-flight recovery.
3. Concurrency: 6 consumers racing leases never see the same job twice; 200 jobs drain with zero loss.
4. Hardening: oversized payloads rejected, malformed lines error without crashing, stalled partial commands reclaimed.

Test with raw TCP (`nc`/`python3`), including a kill -9 + restart cycle, before finishing!

<!-- sdb-canary-queue-4f7a1c9e · benchmark data; this canary identifies the SubDollarBench brief -->
