# 🏆 SubDollarBench: Autonomous Under-$1 LLM Systems Benchmark

> **Measuring under-$1/1M token LLMs on autonomous, production-grade SWE systems engineering tasks (Redis & HTTP/1.1) in isolated Docker sandboxes.**

*Last Updated: 2026-09-10 10:54:29 UTC*

## 📑 Quick Navigation
- [📊 Global Leaderboard](#-global-leaderboard)
- [⚡ Task 1: In-Memory Redis Server](#-task-1-in-memory-redis-server-task-redis)
- [🌐 Task 2: HTTP/1.1 Web Server](#-task-2-http11-web-server-task-http)
- [🖥️ Testbed Environment Specs](#%EF%B8%8F-benchmark-testbed-environment-specs)
- [🚀 How to Reproduce & Submit Results](#-how-to-reproduce--submit-your-results)

---

## 📊 Global Leaderboard

| Rank | Model | Effort | Task | Lang | Pass Rate | Duration | Turns | Throughput | Cost (USD) | Efficiency | Full Trace & Code |
|:---:|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| 🥇 1 | `openrouter/meta/muse-spark-1.3-contributor` | `auto` | HTTP/1.1 | `Python` | 100% (5/5) | 55s | 7 | 25721 req/s *(81% ref)* | $0.0025 *(eff: $0.0101)* | **258.8** pts/¢ *(base: 98.6)* | [Inspect](runs/http_openrouter_meta_muse-spark-1.3-contributor_20260910_105330/) |
| 🥈 2 | `openai/gpt-5.6-luna-pro` | `auto` | HTTP/1.1 | `Python` | 100% (5/5) | 1m 29s | 4 | 5853 req/s *(18% ref)* | $0.0167 *(eff: $0.0279)* | **48.4** pts/¢ *(base: 35.8)* | [Inspect](runs/http_openai_gpt-5.6-luna-pro_20260907_160545/) |
| 🥉 3 | `openrouter/meta/muse-spark-1.3-contributor` | `auto` | Redis | `Go` | 100% (4/4) | 1m 48s | 27 | N/A | $0.0042 *(eff: $0.0345)* | **28.9** pts/¢ | [Inspect](runs/redis_openrouter_meta_muse-spark-1.3-contributor_20260909_180249/) |
| 4 | `openrouter/google/gemini-3.8-flash` | `auto` | HTTP/1.1 | `Go` | 100% (5/5) | 1m 36s | 9 | 46151 req/s *(139% ref)* | $0.0651 *(eff: $0.0740)* | **51.0** pts/¢ *(base: 13.5)* | [Inspect](runs/http_openrouter_google_gemini-3.8-flash_20260909_163707/) |
| 5 | `openrouter/tencent/hy4-preview` | `auto` | HTTP/1.1 | `Python` | 100% (5/5) | 6m 22s | 33 | 14802 req/s *(47% ref)* | $0.1283 *(eff: $0.1809)* | **10.7** pts/¢ *(base: 5.5)* | [Inspect](runs/http_openrouter_tencent_hy4-preview_20260909_174509/) |

---

## ⚡ Task 1: In-Memory Redis Server (`task: redis`)

The candidate LLM is instructed to build a production-grade, concurrent Redis clone from scratch.
- **Wire Protocol**: Full RESP2 parser handling pipelined TCP streams, bulk strings, integers, simple strings, and errors.
- **Supported Commands**: `PING`, `ECHO`, `SET` (with `PX` millisecond expiration, `EX`, `NX`, `XX`), `GET`, `DEL`, `EXISTS`, `INCR`, `DECR`, `COMMAND`, `QUIT`.
- **Packaging Freedom**: Working multi-stage `Dockerfile` (automatically built and containerized) or `./start.sh`.
- **Verification**: Automated raw-socket TCP verification test suite + `redis-benchmark -p 6379 -n 5000 -c 10 -t get,set -q`.

| Model | Effort | Lang | Pass Rate | Duration | Turns | Throughput | Cost | Score | Run Archive |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| `openrouter/meta/muse-spark-1.3-contributor` | `auto` | `Go` | 100% | 1m 48s | 27 | N/A | $0.0042 | 28.9 pts/¢ | [runs/redis_openrouter_meta_muse-spark-1.3-contributor_20260909_180249/](runs/redis_openrouter_meta_muse-spark-1.3-contributor_20260909_180249/) |

---

## 🌐 Task 2: HTTP/1.1 Web Server (`task: http`)

The candidate LLM is instructed to build an RFC 7230/7231 HTTP/1.1 web server from scratch.
- **Features**: `GET /` (200 OK), `404 Not Found Handling`, `GET /echo/{str}` with dynamic `Content-Length`, `GET /user-agent` header reflection, `POST` and `GET /files/{filename}` file persistence, HTTP/1.1 socket keep-alive reuse.
- **Packaging Freedom**: Working `Dockerfile` or executable `./start.sh`.
- **Verification**: 5-stage automated conformance suite + `wrk -t2 -c20 -d3s http://127.0.0.1:8080/`.

| Model | Effort | Lang | Pass Rate | Duration | Turns | Throughput | Cost | Score | Run Archive |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| `openrouter/meta/muse-spark-1.3-contributor` | `auto` | `Python` | 100% | 55s | 7 | 25721 req/s | $0.0025 | 98.6 pts/¢ | [runs/http_openrouter_meta_muse-spark-1.3-contributor_20260910_105330/](runs/http_openrouter_meta_muse-spark-1.3-contributor_20260910_105330/) |
| `openai/gpt-5.6-luna-pro` | `auto` | `Python` | 100% | 1m 29s | 4 | 5853 req/s | $0.0167 | 35.8 pts/¢ | [runs/http_openai_gpt-5.6-luna-pro_20260907_160545/](runs/http_openai_gpt-5.6-luna-pro_20260907_160545/) |
| `openrouter/google/gemini-3.8-flash` | `auto` | `Go` | 100% | 1m 36s | 9 | 46151 req/s | $0.0651 | 13.5 pts/¢ | [runs/http_openrouter_google_gemini-3.8-flash_20260909_163707/](runs/http_openrouter_google_gemini-3.8-flash_20260909_163707/) |
| `openrouter/tencent/hy4-preview` | `auto` | `Python` | 100% | 6m 22s | 33 | 14802 req/s | $0.1283 | 5.5 pts/¢ | [runs/http_openrouter_tencent_hy4-preview_20260909_174509/](runs/http_openrouter_tencent_hy4-preview_20260909_174509/) |

---

## 📡 Task 3: DNS Server (`task: dns`)

The candidate LLM is instructed to build an RFC 1035 UDP DNS Server from scratch.
- **Wire Protocol**: Raw UDP query resolver handling RFC 1035 packet headers, question queries, and A-record resolution.
- **Supported Queries**: A record lookups, standard query flags (QR, Opcode, AA, RD, RA, RCODE), dynamic port listening.
- **Packaging Freedom**: Working multi-stage `Dockerfile` (automatically built and containerized) or `./start.sh`.
- **Verification**: 4-stage automated UDP conformance suite + `queryperf` load test.

---

## 🖥️ Benchmark Testbed Environment Specs

All runs recorded in this repository were benchmarked under identical, isolated hardware and sandbox conditions:

| Component | Specification |
|:---|:---|
| **Operating System** | Ubuntu 24.04.3 LTS |
| **Kernel & Architecture** | Linux 7.0.0-31-generic (x86_64) |
| **CPU Model** | AMD Ryzen AI 9 HX 370 w/ Radeon 890M (24 vCPUs) |
| **System Memory** | 54.5 GiB |
| **Docker Runtime** | Docker version 29.1.3, build 29.1.3-0ubuntu3~24.04.2 |
| **Rust Version** | N/A |
| **Python Version** | Python 3.12.3 |
| **Node.js Version** | v24.13.0 |
| **Git Baseline** | Commit `a428e32` (branch `master`) |
| **Benchmark Captured** | 2026-09-10T10:54:29.585174170+00:00 |

---

## 🚀 How to Reproduce & Submit Your Results

Anyone can clone this repository, run benchmarks on their own models or hardware; results are mirrored to the public static portal.

### 1. Prerequisites
- **Docker**: Docker CE 24+ installed and running.
- **Rust**: Rust toolchain 1.80+ (`cargo`, `rustc`).
- **OMP Agent**: `oh-my-pi` installed (`bun install -g @oh-my-pi/pi-coding-agent`).
- **OpenRouter API Key**: Export `OPENROUTER_API_KEY` in your shell.

### 2. Clone the Repository
```bash
git clone https://github.com/username/subdollarbench.git
cd subdollarbench
export OPENROUTER_API_KEY="sk-or-v1-your-key-here"
```

### 3. Run the Benchmark

#### Option A: Interactive Web UI (Recommended)
```bash
cargo run --release -- ui --port 3000
# Open http://localhost:3000 in your browser
```
1. Select an under-$1/1M token model from the live OpenRouter catalog.
2. Select task (`redis` or `http`) and reasoning effort (`low`, `medium`, `high`, `max`).
3. Click **Start Benchmark Run** to watch live thoughts, tool actions, and verifications.
4. Finished runs are exported to the static portal (./portal/) automatically.

#### Option B: Headless CLI
```bash
# Run Redis benchmark
cargo run --release -- run --task redis --model openrouter/meta/muse-spark-1.3-contributor --effort low

# Run HTTP benchmark
cargo run --release -- run --task http --model openrouter/meta/muse-spark-1.3-contributor --effort low
```

### 4. Public Portal Mirror
Each completed run refreshes the static portal and SUMMARY.md automatically. Sync it to your VPS:
```bash
cargo run --release -- portal --out ./portal
rsync -az --delete ./portal/ <vps>:/srv/bench/
```
Your run directory (`runs/<run_id>/`) contains:
- `workspace/`: The complete candidate codebase produced by the model.
- `manifest.json`: Full metrics, token consumption, live spend, and test stages.
- `console.log`: Complete audit trail of thoughts, bash commands, and test outputs.
- `env.json`: Hardware, operating system, and container specifications.
- `README.md`: Self-contained markdown report for that specific run.

---
*Maintained by the SubDollarBench Community. Under-$1 LLMs can build real software.* 🚀
