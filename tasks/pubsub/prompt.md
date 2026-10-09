You are building a real-time WebSocket pub/sub broadcast engine (RFC 6455) from scratch over raw TCP. NO WebSocket libraries (no ws, gorilla, tokio-tungstenite, etc.). Standard library sockets + crypto primitives (SHA-1, base64) only.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional software engineering standards:
1. **Clean Architecture & Modularity**: Separate TCP accept loop, HTTP Upgrade handshake, frame codec, subscription registry, and broadcast fan-out.
2. **Massive Concurrency**: Sustain 1,000+ concurrent subscriber connections with non-blocking fan-out. A slow subscriber must never block delivery to others (per-connection queues with drop-or-buffer policy, no global locks held during socket writes).
3. **Resilient Error Handling**: Partial frames, abrupt disconnects, unmasked client frames (protocol violation -> close), oversized payloads, and ping/pong mishandling must not crash the engine or leak connections/memory.
4. **Idiomatic Style & Best Practices**: Clean, idiomatic code for your chosen language.
5. **Production Readiness**: Production-grade, maintainable, performant, self-contained. Watch memory per connection.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, curling, scraping, cloning, reading, or consulting the source code of any external WebSocket implementation.
- **Allowed Resources**: RFC 6455 text, your knowledge of SHA-1/base64/framing.
- **Originality**: All code must be your own original implementation. Any access to external WebSocket source code invalidates the benchmark run.

### Programming Language Freedom & Packaging (Dockerfile or start.sh)
- **Choose ANY Programming Language Freely**. Favor runtimes with cheap concurrency (Go goroutines, Rust tokio, Node.js event loop).
- **Pre-installed Toolchains**: Rust (cargo), Go, Python 3, Node.js, C/C++ in PATH.
- All code and configuration must reside in the current workspace.
- **Entrypoint (either)**:
  1. **Working Dockerfile (Recommended)**: exposes port 8092.
  2. **Executable `./start.sh`**: launches your engine on `0.0.0.0:8092`.

### Wire Protocol (TCP port 8092)
1. **Handshake**: client sends `GET /chat HTTP/1.1` with `Sec-WebSocket-Key`; reply `101 Switching Protocols` with `Sec-WebSocket-Accept: base64(sha1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))`.
2. **Subscribe**: client sends masked TEXT frame `SUB <channel>`; server registers the connection.
3. **Publish**: any client sends masked TEXT frame `PUB <channel> <message>`; server broadcasts unmasked TEXT frame `PUB <channel> <message>` to every subscriber of `<channel>` (including, optionally, the sender).
4. **Heartbeat**: reply `pong` to `ping` frames promptly.
5. Channels/messages: `[A-Za-z0-9._-]` channels; messages up to 4KB.

### Verification Criteria
1. Handshake byte-correctness against the RFC test vector.
2. Broadcast correctness: two subscribers on one channel both receive; other channels receive nothing.
3. Heartbeats: pong replies keep connections alive.
4. Soak: hundreds of concurrent subscribers, 100% delivery, bounded p99 broadcast latency, bounded memory.

Test with raw sockets (python3) — including 200+ concurrent connections — before finishing!

<!-- sdb-canary-pubsub-6c40f1b8 · benchmark data; this canary identifies the SubDollarBench brief -->
