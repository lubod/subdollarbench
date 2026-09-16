You are building a high-performance HTTP reverse proxy and load balancer from scratch. Raw sockets only: NO web frameworks (express, flask, gin, axum, nginx libs). The standard library socket + thread/task primitives are your tools.

### Software Engineering (SWE) & Code Quality Standards
Your implementation must meet high professional software engineering standards:
1. **Clean Architecture & Modularity**: Separate client-facing I/O, upstream connection pooling, health checking, routing policy, and header manipulation.
2. **Robust Concurrency**: Handle dozens of concurrent proxied requests without races, deadlocks, or descriptor leaks. Upstream pool access must be thread-safe.
3. **Resilient Error Handling**: Upstream disconnects, slow upstreams, malformed client requests, and disappearing upstreams must never crash the proxy; propagate `502 Bad Gateway` where appropriate, fail over where possible.
4. **Idiomatic Style & Best Practices**: Clean, idiomatic code for your chosen language.
5. **Production Readiness**: Production-grade, maintainable, performant, self-contained.

### CRITICAL INTEGRITY RULE: NO ACCESSING OR CONSULTING SOURCE CODE
- **Strict Prohibition**: You are strictly FORBIDDEN from downloading, curling, scraping, cloning, reading, or consulting the source code of any external proxy implementation (e.g., Nginx, HAProxy, Envoy, Traefik sources).
- **Allowed Resources**: Official HTTP/1.1 documentation (RFC 7230), your knowledge of load balancing algorithms.
- **Black-Box Testing**: You may `curl http://upstream1/`, `http://upstream2/`, `http://upstream3/` (the three upstream reference servers) at any time to observe ground-truth responses.
- **Originality**: All code must be your own original implementation. Any access to external proxy source code invalidates the benchmark run.

### Programming Language Freedom & Packaging (Dockerfile or start.sh)
- **Choose ANY Programming Language Freely** (Go, Rust, Node.js, Python, C++, etc.). Favor strong async I/O and low per-connection overhead.
- **Pre-installed Toolchains**: Rust (cargo), Go, Python 3, Node.js, C/C++ in PATH.
- All code and configuration must reside in the current workspace.
- **Entrypoint (either)**:
  1. **Working Dockerfile (Recommended)**: exposes port 8091.
  2. **Executable `./start.sh`**: launches your proxy on `0.0.0.0:8091`.

### Upstream Topology (fixed, always available during testing)
- Three upstream HTTP servers are reachable from your container at `upstream1:80`, `upstream2:80`, `upstream3:80`, each serving a distinct static body on `GET /`. Always dial these hostnames (port 80). `127.0.0.1:8101-8103` are host-published mirrors for the operator only: from inside your container `127.0.0.1` is yourself and those ports refuse connections.
- Your proxy listens on `0.0.0.0:8091` and forwards client `GET /` requests to the upstreams.
- Self-test tip: for local stub upstreams bind `127.0.0.1:8101` inside your own workspace; never expect the real upstreams there.

### Requirements
1. **Reverse proxying**: `GET /` on :8091 returns an upstream's body with HTTP 200. Preserve the upstream status code and body byte-exactly.
2. **Round-robin balancing**: consecutive requests rotate across all healthy upstreams (30 requests must reach every upstream multiple times).
3. **Upstream keep-alive pooling**: reuse persistent upstream TCP connections; do NOT open a new upstream socket per request.
4. **Header injection**: add `X-Forwarded-For: <client-ip>` (and you may add `X-Proxy-Latency`) to every upstream request.
5. **Failover**: when an upstream dies, detect it and route around it — in-flight bursts must succeed with zero failed requests. When it returns, take it back into rotation. Never let one dead upstream fail the whole proxy.

### Verification Criteria
1. Correctness: status/body fidelity, header presence, valid HTTP/1.1 framing (Content-Length or chunked).
2. Distribution: 30 requests cover all 3 upstreams.
3. Failover: the harness kills one upstream mid-burst; every request must still return 200.
4. Throughput: concurrent GET load (requests/sec).

Test with `curl` and kill/restart cycles of an upstream before finishing!
