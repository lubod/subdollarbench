You are building a durable append-only event broker (Kafka-lite) from scratch.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional software engineering standards:
1. **Clean Architecture & Modularity**: Separate TCP networking, wire protocol parsing, log segment management, and crash recovery. Avoid monolithic scripts or hardcoded shortcuts.
2. **Durability First**: Every acknowledged write must survive `kill -9`. Understand the difference between page cache and durable storage: use explicit fsync/fdatasync (or file flush + OS sync primitives in your language) on the acknowledge path.
3. **Resilient Error Handling**: Handle socket EOFs, disconnects, malformed inputs, partial lines, and torn writes (a crash mid-append must not corrupt prior messages) without crashing or leaking resources.
4. **Idiomatic Style & Best Practices**: Clean, idiomatic code for your chosen language.
5. **Production Readiness**: Production-grade, maintainable, performant, self-contained.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, curling, scraping, cloning, reading, or consulting the source code of any external broker implementation (e.g., Kafka, Redpanda, NATS sources).
- **Allowed Resources**: Official documentation, API references, and your own knowledge of write-ahead log design.
- **Originality**: All code must be your own original implementation. Any access to external broker source code invalidates the benchmark run.

### Programming Language Freedom & Packaging (Dockerfile or start.sh)
- **Choose ANY Programming Language Freely** (Go, Python, Rust, Node.js, C, C++, etc.). Pick what maximizes reliability and throughput.
- **Pre-installed Toolchains**: Rust (cargo), Go, Python 3, Node.js, C/C++ in PATH.
- All code and configuration must reside in the current workspace.
- **Entrypoint (either)**:
  1. **Working Dockerfile (Recommended)**: builds your broker, exposes port 6390.
  2. **Executable `./start.sh`**: launches your broker on `0.0.0.0:6390`.
- **Persistence directory**: store ALL log data under `./data` (relative to workspace). The harness restarts your server in the same workspace after killing it: on startup you MUST replay `./data` and recover every acknowledged message. Do not store state anywhere else.

### Wire Protocol (TCP port 6390, LF-terminated lines)
1. **PUB**:
   - `PUB <topic> <payload>` -> `+OK <offset>` where `<offset>` is the message sequence number in that topic, starting at 0.
   - Topics/payloads: `[A-Za-z0-9._-]` topics, payloads up to 4KB without newlines.
   - Durability: fsync the log BEFORE replying `+OK`. A message you acknowledged must be returned by later SUBs even after `kill -9` + restart.
2. **SUB**:
   - `SUB <topic> <offset>` -> stream `:<off> <payload>` lines for every message with sequence >= offset, then `+END`.
   - Unknown topic -> `+END` immediately.
3. **Errors**: malformed commands -> `-ERR ...`.
4. **Crash recovery**: on startup, scan `./data`, discard any torn trailing record, rebuild offsets. Prior acknowledged messages must be intact with identical offsets.

### Verification Criteria
1. Byte-exact protocol tests: PUB/SUB roundtrip, offsets, multi-topic isolation.
2. Crash test: the harness publishes thousands of messages, `kill -9`s your server, restarts it, and verifies zero acknowledged-message loss with identical offsets.
3. Throughput: pipelined PUB burst (messages/sec).

Test with raw TCP (`nc`/`python3`) including a kill -9 + restart cycle before finishing!

<!-- sdb-canary-wal-9b3e7c25 · benchmark data; this canary identifies the SubDollarBench brief -->
