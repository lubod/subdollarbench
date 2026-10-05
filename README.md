# 🏆 subdollarbench

> **Autonomous, Under-$1 Systems Engineering Benchmark for Budget & Flash LLMs**

`subdollarbench` is a reproducible, fully isolated benchmarking framework designed to evaluate budget / flash LLMs (< $1.00 / 1M tokens) on real-world systems programming tasks.

Instead of evaluating trivial single-function code snippets ([HumanEval](https://github.com/openai/human-eval)) or relying on heavyweight, multi-hour git patches on legacy repos ([SWE-bench](https://www.swebench.com/)), this benchmark challenges models to **autonomously design, implement, debug, package, and optimize complete protocol-compliant network servers from scratch**.

---

## 🏗️ System Architecture

```text
+-----------------------------------------------------------------------------------------+
|                                    HOST ENVIRONMENT                                     |
|                                                                                         |
|   +--------------------------+                      +-------------------------------+   |
|   |   Web UI / CLI Harness   | <==================> |      Run Archiver & Git       |   |
|   | (Axum SSE, Tokio, Clap)  |                      | (runs/<id>/, portal, results) |   |
|   +------------+-------------+                      +-------------------------------+   |
|                |                                                                        |
+----------------|------------------------------------------------------------------------+
                 | Docker Socket Management
                 v
+-----------------------------------------------------------------------------------------+
|                              DOCKER SANDBOX ISOLATION LAYER                             |
|                                                                                         |
|  +---------------------------+  private run network      +----------------------------+  |
|  |     OMP Agent Sandbox     |  subdollar-net-<run_id>   |  Ground-Truth Reference    |  |
|  |   'subdollar-sandbox'     | ------------------------> | (redis:alpine / nginx)    |  |
|  | (Rust, Go, Node, Python,  |  ref-redis / ref-http     |  pinned digests under     |  |
|  |  GCC, Bun, OMP runtime)   |  aliases                  |  stable aliases)          |  |
|  +-------------+-------------+                                                          |
|                | produces candidate code                                                |
|                v                                                                        |
|  +---------------------------+                          +----------------------------+  |
|  |    Candidate Container    | <----------------------- |    Verification Harness    |  |
|  |    'subdollar-candidate'  |     tcp:6379 / 8080      | (Redis RESP2 / HTTP 1.1)   |  |
|  | (Runs model's Dockerfile  |                          |            +               |  |
|  |   or ./start.sh binary)   |                          | (redis-benchmark / wrk)    |  |
|  +---------------------------+                          +----------------------------+  |
+-----------------------------------------------------------------------------------------+
```

---

## 🌟 Core Innovations

1. **Model Selects the Language & Architecture:**
   The model has complete freedom to choose any programming language (Go, Rust, Python, Node.js, C/C++) and architecture. This directly tests SWE pragmatism—balancing execution speed, memory safety, concurrency models, and compiler feedback loops.

2. **Dual-Tier Docker Isolation & Private Run Network:**
   - **OMP Agent Sandbox (`subdollar-sandbox`):** The LLM operates strictly inside an isolated Docker container with pre-installed toolchains (Rust, Go, Node, Python, Clang/GCC, Bun) and workspace volume mounting. The agent is attached to a **per-run user-defined Docker network** (`subdollar-net-<run_id>`) — never the host network — so loopback-bound host services (including the UI at its default `127.0.0.1` bind) are unreachable from model-executed code. Note: services deliberately exposed on `0.0.0.0` remain reachable via the bridge gateway, as from any container on the host.
   - **Ground-Truth Reference:** A pinned reference server runs on the same private network under a stable DNS alias — `redis:alpine` as `ref-redis`, `nginx:alpine` as `ref-http` (redis/http tasks; dns removed 2026-10-05, code+prompt+verifier deleted, data archived — see note). Batch-anchored tasks (redis, http) share one median denominator per batch with a pre/post drift bracket; per-trial live refs cover the rest. The agent issues black-box requests (e.g. `redis-cli -h ref-redis -p 6379` or `curl http://ref-http/`) to inspect actual protocol responses.
   - **Candidate Isolation (`subdollar-candidate-<run_id>`):** The generated code is compiled and launched in a clean run-scoped container, mapping candidate ports (6379 for Redis, 8080 for HTTP).
   - **Egress filter (`SDB_EGRESS_FILTER=1`, on in production):** the run network is created `--internal` and a per-run allowlist proxy sidecar (openrouter + language registries + github) is the agent's only route out. Raw sockets can still bypass proxy env (documented residual); the API key is readable in-container by design — use per-run capped OpenRouter keys to bound blast radius.

3. **Packaging Freedom (`Dockerfile` or `./start.sh`):**
   The harness automatically detects if the model built a standalone `Dockerfile` (root or nested project directory) and builds a clean container. If no Dockerfile is found, it automatically wraps `./start.sh` inside the multi-language sandbox environment.

4. **Multi-Stage Protocol Verification:**
   Zero-flake, byte-level conformance suites verify protocol compliance without third-party abstraction layers. If tests fail, container runtime logs and diagnostics are automatically captured into the audit trail.

5. **Load & Stress Benchmarking:**
   Servers that pass verification are subjected to automated concurrency stress testing using official benchmark tools (`redis-benchmark` and `wrk`), recording requests per second.

6. **Formula & Live-Spend Cost Accounting:**
   Tracks token consumption with input, output, and context caching discounts. For OpenRouter API keys, directly queries the OpenRouter management API before and after each run to record verified live USD billing deltas.

7. **Production Web GUI & Static Portal:**
   Includes a rich single-page web application featuring live Server-Sent Events (SSE) log streaming, responsive run tables, a tabbed modal code inspector, and an interactive prompt markdown editor. Finished runs refresh a static read-only portal (`./portal/`, secrets redacted) for VPS hosting.

---

## 🎯 Supported Tasks (7 live, 1 removed)

| Task | Description | Throughput axis |
| :--- | :--- | :--- |
| **`redis`** | In-memory key-value database, RESP2 wire protocol (`PING`/`ECHO`, `SET`/`GET`/`DEL`/`EXISTS`, `SET ... PX` TTL, `INCR`/`DECR`) | `redis-benchmark`, batch-anchored denominator |
| **`http`** | RFC 7230/7231 HTTP/1.1 server (`GET /`, 404, `GET /echo/{str}`, `GET /user-agent`, `POST`/`GET /files/{name}`) | `wrk`, batch-anchored denominator |
| ~~**`dns`**~~ | RFC 1035 UDP server — **removed 2026-10-05** (code deleted, rows archived) | no longer present |
| **`pdf`** | HTTP microservice on 8090 extracting text from binary PDF payloads (`POST /extract`) | reported per-trial, never scored |
| **`wal`** | Durable append-only log on TCP 6390 (`PUB`/`SUB`, fsync before ack, `kill -9` recovery) | reported per-trial, never scored |
| **`proxy`** | HTTP reverse proxy on 8091 (round-robin, keep-alive, `X-Forwarded-For`, failover) | reported per-trial, never scored |
| **`pubsub`** | Topic bus with replay (`SUB <topic> <offset>`, `:off payload` + `+END`) | reported per-trial, never scored |
| **`binproto`** | Binary length-prefixed RPC (SDB1) | reported per-trial, never scored |

Only **redis** and **http** have a live reference denominator and therefore a
Throughput Score. The other six tasks rank by Pass@1 / cost (efficiency only).

> **DNS removed 2026-10-05** (retired 2026-09-30, then deleted). Five archived
> rows (09-27–09-28) are preserved in
> `provenance-witness/dns-archive-20261005.tar.gz`; result/DB strings load
> without a variant. Rationale (retained for the record): saturated 5/5×100%
> on a lenient verifier (hard-coded-table satisfiable), unmeasurable mock-spec
> denominator (stale `93.184.216.34` vs correct CoreDNS), unprovable `.invalid`
> forwarding stage.

---

## 🚀 Quick Start

### 1. Prerequisites
- **Docker**: Docker CE 24+ installed and running with socket access.
- **Rust Toolchain**: Rust 1.80+ (`cargo`, `rustc`).
- **OpenRouter API Key**: Export `OPENROUTER_API_KEY` in your environment.

### 2. Build the Multi-Language Sandbox Image
```bash
git clone https://github.com/username/subdollarbench.git
cd subdollarbench
docker build -t subdollar-sandbox -f Dockerfile.sandbox .
```

### 3. Launch the Interactive Web UI (Recommended)
```bash
cargo run --release -- ui --port 3000
```
> ⚠️ **Security Warning**: The Web UI does not enforce authentication. Always bind to loopback (`127.0.0.1`, default). Do not bind to `0.0.0.0` or expose the UI port on untrusted networks, as anyone with network access could trigger benchmark runs, spend your API credits, or modify task prompts.

Open `http://localhost:3000` in your browser. From the UI, you can:
- Browse all models on OpenRouter priced below $1.00 / 1M tokens.
- Choose between `redis` and `http` tasks.
- Configure reasoning effort (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; default `high`).
- Watch live agent thoughts, bash actions, and verification stages stream in real-time.
- Inspect candidate source code in the full-screen modal code browser.
- Finished runs refresh the static portal mirror automatically.

---

### 4. Headless CLI Usage

#### Run an Autonomous Benchmark
```bash
export OPENROUTER_API_KEY="sk-or-v1-your-key-here"

# Run Redis benchmark (default high reasoning effort)
cargo run --release -- run \
  --task redis \
  --model openrouter/deepseek/deepseek-chat \
  --budget-usd 0.50 \
  --max-turns 100

# Run HTTP benchmark with max reasoning effort
cargo run --release -- run \
  --task http \
  --model openrouter/google/gemini-2.5-flash \
  --effort max \
  --budget-usd 0.50
```

#### CLI Options Reference (from `run --help`; abbreviated)
```text
Usage: subdollarbench run [OPTIONS] --model <MODEL>

  -m, --model <MODEL>            OpenRouter model ID (e.g. openrouter/google/gemini-2.5-flash)
  -t, --task <TASK>              [default: redis] [possible values: redis, http, pdf, wal, proxy, pubsub, binproto] (`--task dns` no longer parses: unknown task since removal 2026-10-05)
  -e, --effort <EFFORT>          (off, minimal, low, medium, high, xhigh, max) [default: high]
  -b, --budget-usd <BUDGET_USD>  Maximum cost budget in USD [default: 1]
      --max-turns <MAX_TURNS>    [default: 100]
      --timeout-min <MINUTES>    [default: 30]
      --trials <TRIALS>          Repeated trials for Pass@k (n=9 default; n=3 spans ±40pt) [default: 9]
      --extend-if-unstable       Keep adding trials in batches of 3 while unstable
      --max-trials <MAX_TRIALS>  Trial cap incl. initial batch [default: 7]
      --review-model <MODEL>     End-of-run code-review judge [default: openrouter/meta/muse-spark-1.3-contributor]
```

#### Direct Protocol Evaluation (Standalone Mode)
Test an already running server on a specific port without running the agent:
```bash
# Verify Redis server on port 6379
cargo run --release -- eval --task redis --port 6379

# Verify HTTP server on port 8080
cargo run --release -- eval --task http --port 8080

# dns removed 2026-10-05: eval --task dns returns unknown-task error
```

#### Inspect Leaderboard
```bash
cargo run --release -- leaderboard
```

#### Export Static Portal
```bash
cargo run --release -- portal --out ./portal
rsync -az --delete ./portal/ <vps>:/srv/bench/
```


---

## 💰 Pricing Engine & Efficiency Scoring

Parent rows aggregate **all** trials (pass or fail). The headline score is an
expected-utility form — pass rate scaled by Pass@1, divided by the mean trial
cost — i.e. the inverse of **cost per solved task**:

$$\text{Efficiency Score} = \frac{\text{Pass@1 (\%)}}{\text{Mean Effective Cost (Cents)}} = \frac{100 / (\text{cost per solved task in \$})}{100}$$

Pure expected utility: the score of a 100%-at-that-cost run, scaled by the
probability a single trial solves (Pass@1). Mean pass rate is reported
alongside (headline Pass Rate column) but does NOT enter the score — it
would double-count failure, since Pass@1 already prices it. (Pre-`.3` docs
wrote pass% × P@1; the code always did P@1 alone — verified on mixed rows,
e.g. qwen 55.6/0.1465 → 3.8 = P@1/eff.)

Failed trials still burn API + compute, so the mean is over every trial, not
just passing ones (since `2026-10-03.1`; the old passing-only mean understated
spend by a median 1.44x). Failure is priced once, in the Pass@1 multiplier.

Where **Effective Cost ($C_{\text{eff}}$)** is API billing plus a compute floor:

$$C_{\text{compute}} = (T_{\text{seconds}} \times \$0.00003) + (N_{\text{fresh}} \times \$0.00000005) + (N_{\text{cached}} \times \$0.00000005 \times 0.25)$$
$$C_{\text{eff}} = C_{\text{API}} + C_{\text{compute}}$$

- **Time Rate**: $0.00003 / sec ($\approx \$0.108 / hr$ container compute equivalent).
- **Token Baseline**: $0.00000005 / token ($0.05 / 1M tokens); cache-read tokens at 0.25x.
- **Live OpenRouter Accounting:** with an API key, the harness diffs `/api/v1/auth/key` spend before/after each run; the formula is the fallback.

### ⚡ Throughput-Adjusted Scoring (redis + http only)

$$\text{Speed Ratio} = \frac{\text{Throughput}_{\text{candidate}}}{\text{Throughput}_{\text{reference}}}$$
$$\text{Performance Multiplier} = \text{Speed Ratio} \times (1.0 + 2.0 \times \text{Speed Ratio})$$
$$\text{Throughput Score} = \text{Base Efficiency} \times \text{Performance Multiplier}$$

At $\text{Speed Ratio} = 1.0$ the multiplier is $3.0\times$. The reference
denominator is a batch-anchored median with a pre/post drift bracket; a
drifted bracket keeps the candidate's own rate reported but unscored.

### Comparability: the Rules stamp

Every result carries `rules_version = {RULES_VERSION}+{content-hash}` (e.g.
`2026-10-03.1+24e3310c`). The hash covers prompts, verifiers, **and** the
scoring path. Ranks are only comparable within one full stamp — the leaderboard
and matrix gate refuse cross-regime ranks. A version bump means: re-run, or
`backfill --yes` to re-score stored trials onto the new stamp.

> Deferred decisions with evidence (curve, DNS, effort, egress, compute floor,
> review-judge scope) live in [`docs/DECISIONS.md`](docs/DECISIONS.md) — one entry
> each, status + next trigger.


### 🧠 Reasoning Effort Is A Request, Not A Guarantee

`--effort` is forwarded to the agent as an explicit thinking level and is
honoured **by some models and not others**. It is therefore **not a controlled
variable** across the model set, and a wave that varies it is not comparing
like with like.

Measured 2026-10-01 on `binproto`, one trial per cell, `--trials 1`:

| model | low total tok | high total tok | Δ total | Δ **completion** |
|---|---|---|---|---|
| muse-spark-1.3-contributor | 72,284 | 190,287 | +163% | **+145%** |
| glm-5.3-flash | 169,322 | 243,114 | +44% | **+105%** |
| deepseek-v4.1-flash | 185,019 | 180,865 | −2% | −3% |
| qwen3.8-flash | 455,829 | 420,765 | −8% | **+54%** |

Two results worth keeping:

1. **deepseek-v4.1-flash ignored the flag.** It returned the same turn count
   (13) and within 2% of the same token volume at both settings. The flag was
   delivered — the run log records `thinking=low` and `thinking=high` — and had
   no effect.
2. **Total tokens is the wrong instrument for measuring thinking effort.** qwen
   is the clearest case: its total fell 8% while its cost rose 9%, which reads
   as "effort did nothing and got more expensive". Completion tokens show the
   real effect, +54%. Total tokens are dominated by prompt-cache volume, which
   tracks context shape rather than reasoning depth.

**Caveat: every figure above is n=1 per cell.** These are single-trial
observations, not per-model coefficients, and the spread they show is larger
than a single trial can resolve. They are recorded to document *which models
respond at all* — not to quantify by how much.

Consequence for a wave: run a single effort level and record the caveat.
Ranking models at two levels would double the spend to measure a dial that one
model in four does not have.

---

## 📁 Run Traceability & Archival Structure

Every benchmark run generates a self-contained, reproducible archive in `runs/<run_id>/`:

```text
runs/<run_id>/
├── manifest.json      # Structured metrics: tokens, cost, pass rate, stages, duration
├── console.log        # Full console log: agent thoughts, bash tool invocations, compiler logs
├── env.json           # Hardware environment snapshot: CPU, RAM, OS, kernel, Docker & Rust versions
├── README.md          # Markdown report summarizing the run
└── workspace/         # Exact candidate source code produced by the LLM
    ├── Dockerfile     # (or start.sh)
    └── ...            # Source files (main.rs, main.go, server.py, etc.)
```

---

## 🖥️ Reproduction & Submission Workflow

1. Fork and clone this repository.
2. Build the Docker sandbox: `docker build -t subdollar-sandbox -f Dockerfile.sandbox .`
3. Launch the Web UI (`cargo run --release -- ui`) or run via CLI.
4. When a run passes, export the portal and sync it to your VPS:
   ```bash
   cargo run --release -- portal --out ./portal
   rsync -az --delete ./portal/ <vps>:/srv/bench/
   ```
5. Point your domain at the VPS, rsync `./portal/` to `/srv/bench/`, and start the static file service behind your existing Traefik (TLS stays on your current certbot/Traefik setup):
   ```bash
   docker compose -f deploy/docker-compose.yml up -d
   ```

---

## 📜 License
Apache-2.0 or MIT.
