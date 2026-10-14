# Proposal: iterative campaigns — the bench as a developer's week

Updated 2026-10-14 (v4): + CI/testing stage (80% coverage with teeth).

## TL;DR — the ladder in one look

One living codebase, 11 ordered tickets, cumulative acceptance, cost-first
scoring, runtime faults only. Stage i passes only if suites 0..i all pass.

| # | Type | One-liner | What it proves |
|---|------|-----------|----------------|
| 0 | genesis | build the RESP KV server from scratch (stdlib only) | raw systems competence |
| 1 | feature | expiry/TTL semantics | spec fidelity on edge cases |
| 2 | feature | pub/sub | concurrency + state isolation |
| 3 | feature | AOF durability under SIGKILL | crash safety |
| 4 | incident | disk full (ENOSPC injected) | graceful degradation, diagnosis |
| 5 | observability | exact INFO counters + structured logs | instrumentation discipline |
| 6 | engineering | tests with >=80% coverage that actually catch regressions | test discipline |
| 7 | product | web dashboard (HTTP + SSE, offline) | polyglot surface without frameworks |
| 8 | security | AUTH, caps, fuzz, slow-loris, memory cap | adversarial thinking |
| 9 | perf | RESP pipelining, >=1.3x own baseline gate | performance engineering |
| 10 | enterprise | 3-node HA: replication, sync-ack, failover, rejoin | distributed systems |

Headline: **regression-free tickets solved / campaign effective $**.
Survival curve + cost curve report the rest. Perf is a gate, never a
multiplier. Over-budget = `reached`, not failure.

## Operator decisions

1. **Green-field start**: stage 0 is a from-scratch build; the model picks
   the language/architecture.
2. **Stdlib only**: no heavy frameworks, no protocol/database libraries,
   no prebuilt daemons. Raw sockets + language standard library.
3. **11 stages** (0..10): cumulative — every stage re-runs all previous
   acceptance suites; breaking an earlier suite is a *regression*.
4. **Runtime fault injection** is in scope (disk-full, SIGKILL, memory
   cap, fuzz/slow-loris, network partition).
5. **Cost is the headline metric.** Perf stages are pass/fail gates, not
   multipliers; nothing trades $ against speed in the rank key.
6. **Dashboard + GUI** is a product stage; **3-node HA** is the finale.
7. **Tests are a deliverable**: CI gate at 80% line coverage, enforced
   with a sensitivity check, from stage 6 onward.

## Stage 6 — CI/testing contract (the teeth)

Ticket: "every change must ship with tests; CI gate is 80% line coverage."

Model contract: `./test.sh` runs the suite offline, exits 0 on green,
finishes within a time cap (e.g., 120s), deterministic across two runs.
No test frameworks are required beyond the language stdlib.

Harness verification (never trusts the model's own number when it can
compute it):
1. **Run green**: `./test.sh` passes on HEAD, twice (flake check).
2. **Coverage, harness-computed** where tooling exists in the sandbox:
   - Go: `go test -coverprofile` -> parse `coverage: N%`.
   - Node: `node --test --experimental-test-coverage`.
   - Python: `coverage` preinstalled in the image.
   - Other languages: `./coverage.sh` self-report + the sensitivity check
     carries the weight (documented limitation).
3. **Sensitivity check (the anti-fake)**: in a scratch copy of the repo,
   revert the diff of an earlier feature stage (sentinel: stage 1 expiry
   and stage 5 observability), then run `./test.sh`. It MUST fail. A suite
   that stays green when a feature is removed is decoration, not tests.
   Compile failure after revert also counts as caught (conservative).
4. **Persistent gate**: from stage 6 onward, every later stage must keep
   `./test.sh` green and coverage >= 80% — new features without tests
   surface as a regression, not a silent pass.

Reported per stage (not score-weighted beyond the gate): coverage %,
test runtime, sensitivity pass/fail. Cheat surface documented: a test.sh
that inspects harness state instead of the code is a compliance violation;
the scanner adds a look for it.

## Stage plan (11 stages, cumulative suite)

| # | Type | Ticket (model-facing) | Acceptance (hidden, deterministic) | Injection |
|---|------|----------------------|-----------------------------------|-----------|
| 0 | genesis | "ship the MVP: RESP2, PING/ECHO, SET/GET/DEL/EXISTS, INCR/DECR, concurrent clients" | handshake, KV semantics, integer replies, error format, concurrency; malformed frame survival | -- |
| 1 | feature | "clients need expiry" | TTL family incl. SET PX/SETEX/GETEX/PERSIST; SET clears TTL, INCR keeps it; negative TTL deletes | -- |
| 2 | feature | "live updates" | SUBSCRIBE/UNSUBSCRIBE/PUBLISH; cross-connection delivery; no delivery after unsubscribe; order | -- |
| 3 | feature | "must survive a reboot" | AOF + fsync-before-ack; boot replay; torn tail tolerated | SIGKILL mid-load |
| 4 | incident | "writes hang, reads OK — AOF disk filled" | reads keep serving; writes error safely; auto-recovery; AOF never corrupt | ENOSPC on small tmpfs |
| 5 | observability | "ops needs visibility" | exact INFO counters (connections, commands, hits/misses, expired, rejected, memory, uptime); structured logs | scripted traffic |
| 6 | engineering | "tests + CI gate: 80% coverage, tests that catch regressions" | `./test.sh` green twice; coverage >= 80%; sentinel reverts go red | test runtime cap |
| 7 | product | "ops wants a web dashboard" | HTTP port; `/` HTML offline-only; `/api/stats` mirrors INFO; `/events` SSE frames; RESP suite green under dashboard load | no egress (CDNs dead) |
| 8 | security | "one client can take us down" | AUTH on RESP **and dashboard**; 8MB arg cap rejected before allocating; fuzz survivable; slow-loris isolation; secret never logged | 64MB cgroup, fuzz corpus, slow clients |
| 9 | perf | "p99 regressed under pipelined load" | RESP pipelining in order; >=1.3x own stage-0 baseline, p99 budget | pipelined load under BenchLock |
| 10 | enterprise | "we need 3-node HA" | replication + sync-ack + failover + rejoin (contract below) | SIGKILL master, partition node, node rejoin |

## Stage 7 — dashboard contract

- HTTP server (separate port, 8090 + worker offset) alongside RESP 6379.
- `GET /` -> HTML, no external resources (sandbox has no egress).
- `GET /api/stats` -> JSON mirroring INFO fields; harness drives scripted
  traffic, then requires exact counter deltas.
- `GET /events` -> SSE stream; harness connects, generates traffic,
  requires at least one `data:` JSON frame within 2s.
- Full RESP suite runs concurrently with a streaming dashboard client.
- Stage 8 then requires the dashboard behind the same AUTH password.

## Stage 10 — HA contract (the enterprise finale)

Launch contract: `./start.sh --port <p> --data <dir> [--replicaof host:port]`;
three processes 7001/7002/7003 (+ worker offset), separate data dirs;
`ROLE` returns `master`/`replica`.

- **Replication**: writes readable on replicas within 3s; replicas serve reads.
- **Sync-ack**: a write may only be acknowledged after >=1 replica
  confirmed it. Test: N acked writes, then kill the master.
- **Failover**: within T seconds exactly one survivor reports
  `ROLE master`, holds every acked write, accepts new writes that reach
  the other node.
- **Rejoin**: restart the old master; it returns as a replica, never
  co-master, and converges.
- **Regression**: the full accumulated suite passes against the cluster's
  current master (including `./test.sh` from stage 6).
- **Partition (stretch)**: isolate a minority; majority keeps serving;
  minority cannot elect itself master; on rejoin it converges.

## Cost-first scoring

Per stage: pass/fail, turns, wall-clock, API+compute cost; campaign stops
at the budget cap and records `reached`.

- Headline: **regression-free tickets solved / campaign effective $**.
- Supporting: cost curve (cumulative $ per stage), marginal $/ticket,
  survival curve P(stage >= i), diff churn (reported, not scored).
- Perf gate only; no speed multiplier. Free/paid split stays.
- HA is the expensive stage; survival scoring means models that cannot
  afford/reach it are not penalized beyond `reached`.

## Cross-task inspiration (what the siblings already teach us)

| Task | Borrow | Feeds |
|------|--------|-------|
| **wal** | fsync-before-ack; ack accounting (partial acks kept); kill -9 recovery | stage 3; the sync-ack counter for HA |
| **proxy** | pause/unpause kills; failover + rejoin proven through the client path; health gate | stage 10 failover/partition harness |
| **pubsub** | seeded random keys (anti-hardcode); offset/replay semantics | stages 1-2 |
| **binproto** | length-prefix framing edge cases | stage 8 fuzz corpus |
| **http** | same-socket keep-alive; slow-close robustness | stage 7 SSE; stage 8 slow-loris |
| **pdf** | per-request fixtures (distinct nonces) | anti-hardcode for INFO counters |

Rule of thumb: prove through the client-visible path; keep partial fault
evidence; fresh randomness per run; faults reversible and loud.

## Alternative campaign apps (pecking order)

1. **sdb-log** (wal + pubsub fused): durable log -> subscriptions +
   offsets -> retention/compaction -> crash matrix -> replication -> HA.
2. **sdb-edge** (proxy): routing, health checks, balancing, caching/ETag,
   rate limits, TLS, dashboard. Failure machinery already exists.
3. **sdb-http**: files/uploads, Range/conditional GET, compression,
   streaming, auth, caps, dashboard.
4. **sdb-wire** (binproto): v1 -> v2 evolution with backward compatibility
   as the headline.
5. **sdb-doc** (pdf): decompression bombs, malformed structures; niche.

## Harness changes

| Need | Reuse | Add |
|------|-------|-----|
| Stage loop | BenchmarkPipeline per stage, persistent workspace | campaign config + CLI, stage manifests, rollup |
| Cumulative suite | verifier stage functions | stage tags, union 0..i, per-stage verdict vector |
| Code continuity | workspace snapshots | git init + commit per stage; diffs; sentinel reverts |
| Tests stage | git history | `./test.sh` contract, coverage adapters (Go/Node/Python), sensitivity checker, runtime cap |
| Multi-process stages | multi-container launch, worker offsets | N-instance launch, readiness per node, node kill/rejoin |
| HTTP/SSE probes | HttpVerifier style | stats-mirror check, SSE parser, HTML offline check |
| Incidents | kill/restart, pause/unpause | tmpfs ENOSPC, cgroup memory cap, fuzz corpus, slow-loris, partition |
| INFO/logs | log capture | INFO parser + counter deltas; log-secret grep |
| Perf gates | redis-benchmark, BenchLock, n=100k | pipelined client (-P), self-baseline ratio, gate logic |

RULES: campaign spec, stage prompts and suites join GRADED_FILES -> new
stamp. Existing single-shot boards stay untouched; campaign is a separate
board.

## Guardrails

- Calibrate with a strong model: every stage solvable; drop degenerate
  stages.
- Flake bar: new stage test must pass 20/20 before entering a campaign;
  injectors idempotent + reversible; `./test.sh` deterministic (two runs).
- Fresh agent session per stage, same workspace volume; git is the handoff.
- Budget cap is a real stop; `over_budget` visible, never a model failure.
- One campaign per wave; freeze stage specs during it.

## Rollout

1. Slice A: campaign config/CLI + stage loop + git + rollup; tag the
   existing redis verifier into stages 0/1.
2. Slice B: stages 1-2 suites (expiry, pubsub).
3. Slice C: stages 3-5 (AOF, ENOSPC injector, observability).
4. Slice D: stage 6 CI (test.sh contract, coverage adapters, sensitivity
   checker) + stage 7 dashboard (HTTP/SSE probes).
5. Slice E: stage 8 security + stage 9 perf + stage 10 HA.
6. Canary: one cheap model, full ladder; prove the regression alarm;
   publish the cost curve. Then the model grid, then sdb-log (#2).

## Bench-side note

The bench's own CI gate is currently `--fail-under-lines 50` (measured
~70%). If the intent was also to raise the *repo's* gate: set it to 80 and
keep it there; Slice A ships with its own tests so the gate holds.
