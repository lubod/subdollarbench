# Campaigns — next evolution roadmap (2026-10-14)

The bench becomes a **developer's week**: one living codebase, ordered
tickets, cumulative acceptance, cost-first scoring, runtime faults.
See `docs/PROPOSAL-iterative-campaigns.md` (ladder + contracts) and
`docs/PROPOSAL-scoring-v2.md` (protocol/curve/Pareto).

## Baseline evidence — canary wave under 2026-10-14.1

First coherent dataset after the review fixes (6 cells, redis, high,
stop-on-pass, feedback=2, free models):

| model | harness | n | P@1 | GET tp | ref | drift | eff $ |
|---|---|---|---|---|---|---|---|
| ling-3.1-flash | omp | 1 | 100 | 130,548 | 250,000 | clean | 0.1403 |
| ling-3.1-flash | opencode | 1 | 100 | 107,759 | 271,739 | flagged | 0.1194 |
| inkling:free | omp | 2 | 50 | 189,394 | 257,069 | clean | 0.0445 |
| inkling:free | opencode | 1 | 100 | 137,552 | 276,243 | clean | **0.0185** |
| nemotron:free | omp | 2 | 0 | -- | -- | -- | 0.0565 |
| nemotron:free | opencode | 4 | 0 | -- | -- | flagged | 0.0901 |

What it proves: the instrument is stable (refs 250-276k; every passing
cell measured through the new GET window where the broken build measured
nothing); drift quarantine fired on 2/6 brackets; non-result machinery
worked live (agent_failed x2 + disqualified_cheat x1 excluded from n);
equivalent 100% passes cost **7.6x apart** ($0.0185 vs $0.1403) — the
cost-first thesis has signal.

## Decisions taken (operator)

1. Green field, from scratch, **stdlib only** (no frameworks, no RESP/DB
   libraries, no prebuilt daemons).
2. **11 stages** (0..10), cumulative acceptance; regressions cardinal.
3. **Runtime fault injection** in scope (signals, filesystem, cgroups,
   traffic, partition).
4. **Cost is the headline** (regression-free tickets per $); perf is a
   gate, never a multiplier.
5. **Tests are a deliverable**: 80% coverage enforced with a sensitivity
   check (revert sentinel -> suite must go red).
6. Dashboard/UI stage + **3-node HA finale** (auto-failover contract).
7. **Campaign portfolio (compose, don't 1:1)**: single-shot tasks retire to
   genesis/history; campaigns absorb them as capability stages. See the
   portfolio table below.

## Slice A — campaign core

Shipped 2026-10-14 (`src/campaign.rs`, `main_campaign.rs`, pipeline
plumbing; suite 382/0): spec+loader+validation, stage loop with workspace
continuity, per-stage prompt override, acceptance-subset verdicts,
regression detection with severity weights, git-per-stage (commit +
patch + stat), rollup + `sdb campaign {list,dry-run,run}` with
`over_budget`. Open: the live canary gate below (needs spend) and
non-redis acceptance adapters.

- [x] A1. `src/campaign.rs`: CampaignSpec/StageSpec (versioned serde),
  loader + validation (dense ids, prompts exist, acceptance known),
  malformed-spec tests. `campaigns/sdb-kv/campaign.json` is the fixture.
- [x] A2. Stage loop on `BenchmarkPipeline`: workspace continuity (skip
  wipe on stage>0), per-stage prompt override (campaign path), READY
  section reused, run_id `<campaign>_s<N>`, per-stage budget/turn caps.
- [x] A3. Cumulative acceptance: verifier stage-subset filter (redis
  stages by id); stage verdict = suites 0..i; regression detection with
  severity weights (genesis core > feature > metrics). Other tasks: later.
- [x] A4. `git`-per-stage: init after S0 verification; commit after each
  stage (`stage N: <title> [verdict]`); diff patch + stat archived per
  stage; churn on the rollup; judge reads diffs (wiring later).
- [x] A5. Rollup + CLI: `runs/<campaign>/campaign.json` (per-stage
  verdict/cost/turns/duration; regression-free tickets / $; survival
  vector; `reached`); `sdb campaign {list,dry-run,run}`; stop-on-budget
  records `over_budget`, never a model failure.
- [x] A6. Tests: mock-executor orchestrator tests; loader/validation;
  git helper; e2e eval-only CLI smoke (evidence tree verified).
- [x] Gate, part 1 (no spend): `--workspace` seed + full-ladder eval-only
  replay of the archived ling Go candidate — 4 tickets passed
  cumulatively (3/3 -> 4/4 -> 5/5 -> 6/6), non-accepted suites excluded,
  destructive AOF kill only at s3, git commits + patches + rollup
  (reached 3, tickets/$ computed). Regression logic proven by
  mock-executor tests (pass->fail flips, fault status, budget stop).
- [x] Gate, part 2 (spend) — **regression alarm proven live** (2026-10-14,
  ling, stages 0-1): ticket 0 PASS (3/3, $0.0610, 40 turns, git e46ed9d);
  ticket 1 FAILED with real regressions: the model exhausted its 25-turn
  pool mid-refactor, its last-minute full-file rewrite shipped a compile
  error (`ok declared and not used`), the server never started, and
  [1, 2, 4] were flagged as regressions (sev 3); the failing state is
  committed as ef63f1b [fail]. Ladder stopped at reached=0. Rollup:
  $0.1095, regression-free 1, tickets/$ 9.1. Fail taxonomy for this
  canary: budget-exhaustion + no-compile, not wrong expiry semantics.
- Calibration finding for D6: `max_turns` IS the shared pool (initial
  cap = min(max_turns, 50) with feedback on), so a 25-turn ticket leaves
  zero repair headroom — the repair round never ran ("Global turn budget
  reached across attempts"). Re-run after D6 is settled.
- Gate, part 3 — D6(b) validated live (2026-10-14, ling, run 2): all four
  tickets PASS — s0 3/3 (30 turns), s1 4/4 (19), s2 5/5 (19), s3 6/6
  (23 initial + 7 repair = 30). Reached 3, regression-free 4, $0.1743,
  tickets/$ 22.9. The repair round converted a real failure: ticket 3's
  initial attempt hit the cap mid-WAL with three comment lines mangled by
  the edit tool (compile error, 0/6 verification), and the 7-turn repair
  fixed exactly those lines -> 6/6. Candidate measured 108k GET req/s;
  write probe 816 SET/s.
- **D6(b) amendment (2026-10-14, later): repair floor 8 -> 12.** After
  three more cells whose repair rounds could not recover an interrupted
  refactor (ling run 4: fixed two compile errors then hit the limit
  mid-fix; ling run 5: surveyed 7 turns, one edit at the cap; inkling
  probe: never edited), the reserve is now `max(max_turns/4, 12)` capped
  at `max_turns/2`. Initial caps: 40->28, 30->18, 25->13; the half-pool
  cap keeps tiny pools sane (10->5, 4->2, 1->1). RULES `2026-10-14.6`;
  DECISIONS #12
  amended; `initial_turn_cap` tests updated. Watch for small tickets that
  used to pass initially now needing their repair round.
- Finding (Slice F dependency): campaign stages run WITHOUT a batch
  reference, so each passing stage tries the live per-trial ref — at s3
  the ref measurement failed ("Could not connect to ref-redis") and the
  speed reading went unscored. The perf gate needs a shared/batch
  denominator for campaign tickets (or a readiness wait + retry).

## Slice B — stage suites (expiry, pubsub)

- [ ] B1. Expiry suite: TTL family semantics (SET clears TTL, INCR
  preserves, negative/zero deletes, PTTL rounding, GETEX options).
- [ ] B2. Pub/sub suite: seeded channels, multi-channel subscribe,
  unsubscribe silence, publish ordering, receiver counts.
- [ ] B3. Stage-tag the existing redis verifier tests so cumulative
  selection is data, not code.

## Slice C — durability, incidents, observability

- [ ] C1. AOF acceptance: stage 6 covers fsync-before-ack + SIGKILL +
  replay on live tickets; torn-tail and replay-idempotence checks still
  to add (reuse WAL verifier patterns).
- [x] C2. ENOSPC injector (2026-10-14.3, v1): ticket 4 (acceptance
  [1,2,3,4,5,6,8]) adds the `SDB_DATA_DIR` contract; the pipeline restarts
  the candidate with `--tmpfs /data:size=8m` + `SDB_DATA_DIR=/data`
  (`start_candidate_enospc`), fills it with 240KB protocol writes, and
  stage 8 requires: acked data still reads, PING alive, the first
  unpersistable write returns an error and is NOT applied, later writes
  keep answering. Mock tests cover pass + never-full. Deferred sub-checks
  (documented SKIPs): free-space recovery and replay-after-incident need
  to free/reach the container tmpfs -> docker exec (shell must exist) or
  a privileged loop-mount injector; revisit before the perf-gate wave.
  Ticket 5's acceptance now also includes 8 (cumulative incident check).
- [x] C3. INFO suite (2026-10-14.2): verifier stage 7 — required fields,
  integer parse, monotonicity, randomized delta checks (hits/misses/
  commands/connections/expired). Runs ONLY when a campaign ticket
  explicitly accepts id 7; single-shot totals unchanged. Secret-in-logs
  grep lives in E3.
- [x] C4. `parse_info` + delta checks are built into stage 7; candidate
  log-line parsing deferred to E3 (pipeline already captures logs).

## Slice D — tests stage (coverage with teeth)

- [x] D1. `./test.sh` contract (2026-10-14.4): offline (network-none
  container), deterministic (two runs), <=3min per run, exit 0 = green.
- [x] D2. Coverage adapters: Go measured by the harness
  (`go test -coverprofile` + `go tool cover -func`); other languages read
  a self-reported `coverage.txt` ("coverage: NN.N%") — sensitivity check
  carries the anti-fake weight there (documented limitation).
- [x] D3. Sensitivity checker: scratch copy (no .git), stage-1 expiry
  diff reversed from the stage-0 commit, `./test.sh` must fail; compile
  failure counts as caught.
- [x] D4. Persistent gate: ticket 6 ships `./test.sh`; every later ticket's
  acceptance includes id 9 (ticket 6 = [1..9]). Ladder entries for 7+ must
  carry 9 in their acceptance list.
- [ ] D5. Sandbox image: python `coverage` preinstall and/or
  `cargo-llvm-cov` remain optional (only if a future grid model needs a
  real adapter for its language).

## Slice E — dashboard + security

- [ ] E1. Dashboard probes: `/` offline HTML (no external refs),
  `/api/stats` mirrors INFO under randomized traffic, `/events` SSE
  frame <2s, full RESP suite green while a dashboard stream is open.
- [ ] E2. Security suite: AUTH on RESP and dashboard, 8MB argument cap
  rejected before allocation, fuzz corpus (binproto/robustness seeds),
  slow-loris isolation + idle-incomplete timeout, 64MB cgroup.
- [ ] E3. Secret hygiene: password absent from logs and error strings.

## Slice F — perf gate + HA finale

- [ ] F1. Perf gate: RESP pipelining in order; >=1.3x own stage-0
  baseline under pipelined load; p99 budget; BenchLock discipline.
  PREREQ (canary finding): campaign stages must share a batch reference
  or retry the live ref — s3's ref measurement failed ("Could not connect
  to ref-redis") and the speed reading went unscored.
- [ ] F2. Multi-instance launch: N nodes, per-node port/data dir,
  readiness per node, worker port offsets.
- [ ] F3. HA acceptance: `ROLE` contract, replication bound (<=3s),
  **sync-ack zero acked-write loss**, SIGKILL master -> exactly one new
  master, old master rejoins as replica, cumulative suite green on the
  current master.
- [ ] F4. Partition injector (isolation), split-brain rules, rejoin
  convergence (stretch: own incident stage).
- [ ] F5. HA budgets: this stage is expensive; survival scoring prices
  it, do not hide it.

## Slice G — scoring v2 + portal

- [ ] G1. Protocol gating: rank only within (task, rules_version,
  harness, protocol); matrix uniform-protocol rule; row badges.
- [ ] G2. Decide SPRT(0.3,0.8) vs confirmatory top-up (simulation table
  in PROPOSAL-scoring-v2).
- [ ] G3. Throughput curve decision: keep / cap 1.5 / linear /
  report-only.
- [ ] G4. Pareto frontier: pass@1 (Wilson) vs $/solve; floor sensitivity
  0.5x/1x/2x from archived rows; display-only first.
- [ ] G5. Campaign portal view: survival curve P(stage>=i), cumulative
  cost curve, per-stage chips, regression badges, churn.

## Slice H — validation + paper inputs

- [ ] H1. Canary: one cheap model, full 11-stage ladder; publish the
  cost + survival curves.
- [ ] H2. Grid: 4-6 cheap models x sdb-kv; uniform effort/harness; one
  campaign per wave; never mix boards.
- [ ] H3. Failure taxonomy from campaign stages (truncation, tool
  misuse, regression patterns, HA failures).
- [ ] H4. Paper inputs: survival curves, $/ticket, harness deltas,
  regression-free rates.

## Slice I — stage-2 wave: disk residency, compaction, admin protocol

Ladder extended beyond the 11-stage v1 (tickets 11-13; stages 14-16;
RULES `2026-10-15.1`). The store's story continues: the dataset outgrew
RAM, disk filled with append-only history, ops tooling needs a machine
interface.

- [x] I1. Ticket 11 — disk-resident store (stage 14): pipeline restarts
  the candidate with `SDB_DATA_DIR` on a host bind mount under a 192MB
  memory cap; the verifier writes ~2000 x 240KiB values (~480MB) through
  RESP, spot-checks 40 full values byte-for-byte, deletes five keys, then
  the pipeline SIGKILLs + restarts and re-checks. A map-in-RAM candidate
  is OOM-killed. Prompt: `stages/11_diskresid/prompt.md`.
- [x] I2. Ticket 12 — compaction (stage 15): churn of 600 x 240KiB x 6
  (~860MB of records) then 400 deletes (48MiB live); the harness measures
  the host bind mount and requires the footprint <= 3x live bytes (144MiB)
  within 30s, survivors byte-exact, then SIGKILL + restart keeps survivors
  and deletions. Values are hex-of-PRNG (poorly compressible) so a
  compression-only design cannot fake the gate.
- [x] I3. Ticket 13 — admin binary protocol (stage 16): SDB1 v2 on port
  6395 (worker offset), published only when a ticket's acceptance includes
  stage 16. Framing fidelity (bad magic closes, PING/ECHO byte fidelity,
  corrupt CRC -> ERR_BAD_CHECKSUM with the connection kept open, unknown
  opcode -> ERR_UNKNOWN_OP, 50-frame pipeline with matched ids+payloads)
  plus v2 additions: the SAME keyspace as RESP in both directions and
  STATS (opcode 0x06) mirroring the INFO counters.
- [x] I4. Harness: `CandidateRunExtras.binds` (host bind mounts),
  `disk_bytes` footprint walk, seeded big-value helpers, sectioned admin
  stage (a monolithic async fn overflowed 2MiB test stacks in debug; the
  64KiB read buffer in `Conn::read_frames` is heap-allocated now), live
  docker bind round-trip covered in `live_docker_cov_tests`.
- [x] I5. Mock/offline validation: admin stage against faithful SDB1+RESP
  mocks (pass, split-store fail, missing-STATS-field fail); compaction
  gate pass (sparse log shrinks) and loud no-shrink fail; helper/dir-bytes
  unit tests; extras args bind assertion.
- [x] I6a. Eval-only LIVE proof (2026-10-15, no model): a reference mock
  candidate (offset index + append log + idle compaction + SDB1 v2)
  driven through the real pipeline for stages 14-16 — all three PASS
  (`test_pipeline_eval_only_stage2_disk_and_admin_live`, ~110s incl. the
  100%-pass bench): host-bind restart under the 192MB cap, ~480MB
  written/re-read byte-exact across SIGKILL, ~860MB churn reclaimed to
  <=3x live, admin channel with shared keyspace + STATS. The admin port
  for eval-only workers is passed via argv (real worker-0 campaigns use
  6395, matching the published 1:1 port).
- [ ] I6b. Model-driven live execution of stages 14-16 (pending a model
  that reaches ticket 11; the grid currently tops out at ticket 4).

## Slice J — sdb-edge (proxy campaign: routing / resilience / health / cache / limits)

Second campaign portfolio entry (DECISIONS #11 porting order), task
`proxy`, RULES `2026-10-15.2`.

- [x] J1. Task adapter: `campaign.rs` acceptance is per-task now
  (`redis_stages` / `proxy_stages`), the validator range is per task
  (redis 1..=16, proxy 1..=9), and the CLI resolves
  `stage.acceptance.stages_for(task)`.
- [x] J2. Ladder: 7 tickets on one living workspace — t0 genesis (route
  `GET /`, body/status fidelity, framing, X-Forwarded-For), t1 balance +
  keep-alive ([1,2,3]), t2 survive upstream loss ([1-4]), t3 fail fast
  under hangs ([1-4,6]), t4 active health + `/status` ([1-4,6,7]),
  t5 caching ([1-4,6,7,8]), t6 limits under a 128MB cap
  ([1-4,6,7,8,9]). Throughput (stage 5) stays single-shot for now.
- [x] J3. New stages 6-9 in `verifier/proxy.rs` (campaign-only: never run
  for single-shot proxy rows, so that board is unchanged): quick failover
  under a frozen upstream (2500ms per-request budget + retry + rejoin),
  `/status` health surface driven by active probes (flip while idle,
  recovery <=5s), TTL cache (`SDB_PROXY_CACHE_TTL`,
  `X-Cache: MISS|HIT`, `Age`, byte-identical bodies, expiry; the pipeline
  restarts the candidate with the env, then restores), limits
  (32KiB headers -> 4xx/close, declared 100MB body -> 413/close, stalled
  half-request reclaimed <=12s with normal traffic flowing, all under a
  128MB cap via a pipeline restart).
- [x] J4. Mock validation: `/status` parsing + snapshot failures, cache
  gate pass + never-hits / no-Age / body-mismatch / never-expires fails,
  limits pass + careless-proxy fail, helper tests (35 proxy tests green).
- [x] J5a. Live smoke (2026-10-15, no model): a minimal forwarding proxy
  + the run-network nginx pool driven through the real pipeline with
  acceptance [1] — the proxy campaign path works end to end (task
  adapter, pool startup, acceptance-scoped verdict 1/1, stage-log
  contract); bench runs against the candidate.
- [x] J5b. Full-ladder LIVE proof (2026-10-15, no model): the reference
  proxy (`EDGE_MOCK_SERVER_PY` in `src/pipeline.rs`: round-robin,
  upstream timeout+retry, active health probes + `/status`, TTL cache,
  request limits) driven through the real pipeline with acceptance
  [1,2,3,4,6,7,8,9] — all 8 stages PASS
  (`test_pipeline_eval_only_proxy_full_ladder_live`, ~84s incl. the bench):
  docker-pause failover, quick failover under a hung upstream, the health
  surface flipping while idle, the cache restart (`SDB_PROXY_CACHE_TTL`),
  and the 128MB-capped limits stage.
  Harness bug found + fixed by this test: candidate restarts (stage 8/9,
  and the redis disk stages for consistency) did not re-attach to the run
  network (`connect_candidate_to_network`), so a restarted candidate could
  not resolve the upstream aliases (observed as an unbounded getaddrinfo
  stall against the host resolver). The initial start path already
  attached; restarts now do too.
- [ ] J5c. Model-driven grid cells for sdb-edge.
- [ ] J6. Later tickets: TLS termination with a harness-provided cert,
  and a throughput ticket (stage 5 + baseline gate) once proxy baselines
  are recorded.

## Slice K — sdb-queue (the new app: leases / ack / nack / retries / DLQ)

Third portfolio entry (DECISIONS #11 order) and the one genuinely NEW
application — no single-shot task existed, so the campaign is its primary
home. New task type `queue` (default port 6385), RULES `2026-10-15.3`.

- [x] K1. Task type + plumbing: `TaskType::Queue` (9 single-shot stages,
  port 6385), omp task guidance (protocol + anti-cheat), batch-ref None,
  CLI verify/replay arms, `tasks/queue/prompt.md`, and the config
  prompt-validation list.
- [x] K2. Verifier `src/verifier/queue.rs`: line protocol
  ENQ/LEASE/ACK/NACK/REDRIVE/STATS/PING with a contract header; stages
  1-9 (FIFO round trip; visibility + expiry + connection ownership; nack
  immediate/delayed + attempts; DLQ at 3 attempts with inspection-only
  reads; durability prepare/verify split for the pipeline kill; 6-consumer
  no-duplicate race; per-queue STATS; delayed ENQ + redrive; hardening
  with a 32KiB payload, malformed lines and slow-loris), plus a
  `measure_throughput` soak.
- [x] K3. Mock broker in the tests (~250 lines: leases, expiry, DLQ moves,
  redrive, per-queue counters, payload fidelity) with 8 injected defects
  (no-expiry, no-ownership, no-DLQ, duplicate delivery, frozen stats,
  void delays, oversized-ok, never-reclaim) each asserted to fail the
  intended stage with a naming error; plus a faithful-pass test over all
  self-contained stages.
- [x] K4. Pipeline arm: acceptance-scoped stages 1-9; stage 5 is the
  destructive durability half (prepare -> SIGKILL -> restart -> verify,
  skipped for tickets whose acceptance excludes it); stage 9 restarts the
  broker under a 128MB cap; setup/bench arms treat the task as
  verifier-timed (no reference container, jobs/sec soak).
- [x] K5. Campaign spec + prompts: nine tickets (genesis -> leases ->
  nack -> DLQ -> durability -> concurrency -> stats -> delays/redrive ->
  hardening) with cumulative acceptances and a dedicated base prompt.
- [x] K6a. LIVE proof (2026-10-15, no model): a stdlib-only reference
  broker (`QUEUE_MOCK_SERVER_PY` in `src/pipeline.rs`: connection-owned
  leases, nacks/delays, DLQ after 3 attempts with inspection reads,
  redrive, per-queue STATS, delayed enqueue, append-only fsync log under
  `./data` with torn-tail replay) driven through the real pipeline with
  acceptance [1..9] — all 9 stages PASS
  (`test_pipeline_eval_only_queue_full_ladder_live`, ~20s): FIFO round
  trip, lease expiry + ownership, nack immediate/delayed, DLQ, **the
  durability half (prepare -> SIGKILL -> restart -> verify: acked never
  return, in-flight returns with attempts preserved, DLQ intact)**, the
  6-consumer race (200 jobs, zero duplicates), STATS, delayed ENQ +
  redrive, and hardening under the 128MB cap + the jobs/sec soak.
  The live proof caught a real verifier bug: the durability prepare
  leased assuming FIFO would hand it the *prepared* job, but a correct
  FIFO broker returns the earlier *waiting* job — prepare now settles
  each state before enqueueing the next, and a mock test
  (`mock_queue_durability_halves`) covers both halves so it cannot
  regress.
- [x] K6b. First model cell (2026-10-15, ling-3.1-flash): **the full
  ladder — reached 8, 9/9 tickets, zero regressions, $0.2412** (see the
  sdb-queue grid log). More models next.

## Slice L — sdb-http (the last portfolio app: ranges / validators / compression / limits)

Fourth portfolio entry and the final app in the DECISIONS #11 order.
RULES `2026-10-15.4`. Task `http` (port 8080); the single-shot 5-stage
board (root/404/echo/user-agent/files) is unchanged.

- [x] L1. Verifier depth in `src/verifier/http.rs`: stage 6 byte ranges
  (206 + `Content-Range`, suffix/open, 416, plain-GET unchanged), stage 7
  conditional GET (`ETag` + `Last-Modified`, 304 semantics, stale
  validators, rewritten content changes the ETag), stage 8 gzip
  negotiation (decodable + smaller body via flate2; identity passthrough),
  stage 9 chunked uploads + `Expect: 100-continue` (no deadlock) +
  declared-128MiB refused at the head under a 128MB cap, stage 10
  hardening (three traversal spellings, 32KiB heads, malformed lines,
  slow-loris <=12s, liveness).
- [x] L2. Pipeline arm: acceptance-scoped stages 1-5 (single-shot path
  unchanged), campaign-only stages 6-10; 9/10 run after a 128MB-cap
  restart with the run-network re-attach; robustness probes stay advisory.
- [x] L3. Campaign: 8 tickets (t0 root+404 -> t1 echo/user-agent -> t2
  files -> t3 ranges -> t4 validators -> t5 gzip -> t6 chunked uploads +
  limits -> t7 hardening) with `http_stages` acceptance (1..=10) and
  prompts; validator range per task; spec loads and dry-runs.
- [x] L4. Mock rigor: a full mock file server (range slices, validators
  with real date comparisons, gzip via flate2, chunked/continue upload
  paths, traversal guard, head cap, stalled-socket reclaim) with **10
  injected defects**, each asserted caught by the intended stage with a
  naming error, plus a faithful-pass run over stages 6-10.
- [ ] L5. Live proof (eval-only reference file server through the real
  pipeline, incl. the capped restart), then model grid cells.

## Coverage campaign (operator requirement: bench code >= 80% lines)

Measured 2026-10-14 (`cargo llvm-cov --all-targets --summary-only`, the
same command the CI gate runs): **84.65% lines** (regions 85.47%,
functions 88.74%), up from **61.03%** before the campaign. The operator
requirement (bench code >= 80%) is met with margin; the CI gate at 80
enforces it when Actions is re-enabled.
Note: the command counts test code in both numerator and denominator;
production gaps were tracked alongside each wave and the big files were
covered for real (see per-file numbers below).

Wave log:
- Wave 1: pubsub 31->95%, pdf 35->96.6%, proxy 58->88.4%, wal 98.3%,
  binproto 98%, judge 48->95.7%, review_cmd 0->96.9%, repair 7->90.9%,
  cost_annotate 28->57% (+ the total_cost_usd gate fix), CLI handlers,
  web/server 48->77.3%, web/queue 35->67.7%.
- Wave 2 (aggregate 61.0 -> 76.6%): the two web files above + CLI.
- Wave 3: docker 47->85.7%, batch_ref 32->80%, leaderboard 92.2%,
  portal 98.7%, remote.
- Wave 4: pipeline +139 production lines (eval-only mock-candidate
  flows), opencode 95.4%, omp 95.8%, omp_updater 96.5%.
- Wave 4b: main.rs 68.2->81.5%, main_campaign.rs 10.9->69.6%.
- Aggregate after 4b: **84.65% lines** — target met.
- Wave 6 (stage-2 wave, 2026-10-15): aggregate 84.65 -> **84.71% lines**
  (regions 85.55%, functions 88.79%). The eval-only live proof covers the
  stage-14/15/16 orchestration and the disk/compaction verifiers
  (verifier/redis.rs 78.78 -> 82.11% lines; binproto 93.78% incl. the
  admin stage; sandbox/docker.rs 85.39% with the live bind round-trip).
  Gate margin ~4.7pp.
- Wave 7 (sdb-edge proxy campaign, 2026-10-15): aggregate 84.71 ->
  **84.43% lines** (regions 85.27%, functions 88.74%). The dip is the new
  proxy stage-6..9 orchestration blocks (live-only) and the docker-gated
  stages 6/7; verifier/proxy.rs sits at 79.65% lines (cache/limits/status
  logic is mock-covered, pause-based stages need live upstreams),
  campaign.rs 91.73%, pipeline.rs 56.59%. Gate margin ~4.4pp.
- Wave 8 (sdb-edge full-ladder live proof, 2026-10-15): aggregate 84.43 ->
  **84.82% lines** (regions 85.69%, functions 89.09%). The eval-only
  reference-proxy run covers the proxy arm's stage-6..9 orchestration and
  the fixture (pipeline.rs 56.59 -> 59.12%); gate margin ~4.8pp.
- Wave 9 (sdb-queue campaign + live proof, 2026-10-15): aggregate 84.82 ->
  **84.76% lines** (regions 85.56%, functions 88.88%). The new
  verifier/queue.rs lands at **85.77% lines** (mock broker + defect
  matrix + both durability halves + the eval-only pipeline run);
  pipeline.rs rises again with the queue arm and fixture. Gate margin
  ~4.8pp.

Remaining smaller residuals if a future wave wants more margin:
web/server.rs (1,098 incl. test code), web/queue.rs (798),
pipeline.rs (~1,100 prod), verifier/redis.rs (514/578),
verifier/http.rs (244), report/leaderboard.rs (263), cost_annotate,
tool_normalizer, portal_proxy, cutoff_annotate.
Scheduling note: live-docker tests can flake under heavy host load
(parallel agent builds); the suite is green on a quiet host. If CI shows
flakes, serialize live-docker tests with a shared test lock.

## Grid log (sdb-edge, omp, high effort)

- **ling-3.1-flash** (spec tickets 0-6, 2026-10-15 run 1,
  `campaign_sdb-edge_openrouter_inclusionai_ling-3.1-flash_20261015_101524`,
  RULES `2026-10-15.2`, $0.80 cap, 29 min): **reached 4 as recorded
  (reached 3 under the fixed harness — see below)**, $0.1907, 6 tickets
  attempted. t0 genesis PASS first try (stage 1; 4482 req/s candidate
  bench). t1 balance PASS (stages 1,2,3). t2 failover **FAIL -> repaired
  in round 1** (stage 4 read timeout against a frozen upstream; the repair
  added upstream timeouts + retry). t3 quick-failover PASS first try
  (stage 6) — the t2 repair generalized. t4 health PASS in-run (stage 7)
  but **luck**: the verify-time pool churn (harness bug (b) below) had the
  candidate's prober already in unhealthy-recheck mode, which masked that
  it never probes *healthy* upstreams. t5 caching FAIL x2 in-run (stage 8
  read timeout = harness bug (a)), not a candidate failure.
  Clean-harness replays (eval-only, no API cost, fixed binary):
  t4 state -> t0-t3 PASS, **t4 FAIL stage 7 honestly** ("upstream2 was
  down for 5s of idle but /status still says up — probe upstreams
  actively"); final state -> **stage 8 (caching) PASS**, **stage 9 FAIL**
  ("a half-sent request stayed open for 12s: enforce a header/read
  timeout (slow-loris)" — the oversized-header/body rejections passed).
  Honest capability gaps: active health checks (t4), slow-loris timeout
  (t6); caching is correct. Rejections passed: nothing on t0-t3.
- **nemotron-3-super-120b-a12b:free** (spec tickets 0-6, 2026-10-15 run
  1, `campaign_sdb-edge_openrouter_nvidia_nemotron-3-super-120b-a12b_free_20261015_111858`,
  RULES `2026-10-15.2`, $0.50 cap, ~20 min): reached **none** — t0 **FAIL
  0/1** (recorded $0.0329, 24 initial + 12 repair turns; ~74k
  prompt tokens). Its agent tooling worked this time (`write`/`bash` OK,
  unlike the sdb-kv cell) and it produced a 234-line `proxy.py` with
  round-robin + X-Forwarded-For, but the verifier cannot parse its
  response in either round ("Failed to parse HTTP response from
  127.0.0.1:8091"): the hand-rolled forwarder echoes upstream bytes yet
  emits a malformed/partial reply (response-layer failure, not routing).
  Cost nit: the `:free` variant still lands a ~$0.03 cost in the record
  (same as its sdb-kv cell, $0.0312) — the pricing path does not zero
  free-tier variants; open item (either zero it or label the rows as
  token-only).
- **inkling-small:free** (sdb-edge run 1, 2026-10-15,
  `campaign_sdb-edge_openrouter_thinkingmachines_inkling-small_free_20261015_122552`,
  RULES `2026-10-15.2`, $0.50 cap, ~7 min wall): reached **1**
  (2 regression-free tickets), recorded $0.0263 (the free-tier pricing
  nit again). t0 genesis PASS first try (21 turns, $0.0081; left
  `proxy_temp.py`/`proxy_temp2.py` copies in the workspace). t1 balance
  PASS (18 turns, $0.0070; rewrote `proxy.py`). t2 failover **FAIL 1/4**
  (30 turns, $0.0112) with **regressions [2, 3] (sev 3)** — the cell's
  headline finding: the initial attempt had no upstream timeout/retry
  (stage 4: "GET 2 during failover: Read timed out"), and the **repair
  round broke round-robin (stage 2: unparseable response) and keep-alive
  (stage 3: read timeout)**. First repair-induced regression on routing
  stages in the sdb-edge grid; exactly what cumulative acceptance exists
  to catch (the model's own `proxy.py.bak` was 307 lines vs the 272-line
  rewrite).
- **ling-3.1-flash** (sdb-edge run 2, 2026-10-15,
  `campaign_sdb-edge_openrouter_inclusionai_ling-3.1-flash_20261015_121246`,
  RULES `2026-10-15.2`, $0.80 cap, ~22 min): reached **none** — t0 **FAIL
  0/1 both rounds** ($0.0518, 24 + 12 turns). Symptom: the verifier's
  `GET /` connected but never received a framed response (8s exchange
  deadline) in either round, while the candidate's own direct fetches to
  all three upstreams returned 200s and its log shows a threaded proxy
  listening on 8091 (401-line `proxy.py`: thread per connection,
  read/parse/forward with re-framing). => a **client-request-path defect**
  (connect-but-don't-answer for the verifier's plain `GET /` with
  `Connection: close`), not transport or upstream access. Run-to-run
  variance on the genesis ticket is now observed for this model: run 1
  (same spec) passed t0 first try and reached 4 recorded.
  Frontier-note for prompts (proposed, NOT applied — it is a spec change):
  the candidate container carries an `http_proxy` env var pointing at an
  egress proxy; the agent burned ~5 turns discovering `curl` was routed
  through it (`--noproxy '*'` fixed it) although raw sockets were
  unaffected. A prompt line ("tool curl may honor an egress-proxy env
  var; your server must dial `upstreamN` with raw sockets") would remove
  harness noise; applying it means a spec bump.
- Harness bugs this cell found (all fixed, same day):
  (a) candidate restarts did not re-attach to the run network -> a
  restarted proxy could not resolve `upstreamN` (getaddrinfo stall; stage
  8 read timeout). Restart blocks now call
  `connect_candidate_to_network()`.
  (b) the verify-time pool check used **host-published ports** (a harness
  convenience with byte-identical in-process fallbacks) and rebuilt the
  pool before *every* verification — killing the candidate's pooled
  connections and health state right before routing stages. In eval-only
  replays this produced spurious stage-2 `[0,20,10]` / stage-6/7 failures.
  Pool health is now a container-state check (`upstream_container_running`)
  and the pool is rebuilt only when missing/stopped; the misleading
  "unreachable at verify; recreating fresh" churn is gone.
  (c) the proxy bench tried the live-nginx calibration (`ref-http`) even
  though the denominator is always discarded for proxy tasks (misleading
  "[BENCH] live Nginx reference FAILED"); the calibration is now skipped.
  Open question (hygiene): in this environment the upstream containers'
  host publishes (8101-8103) are configured but sometimes have no
  listener (manual `docker run -p` works). Nothing in the graded path
  depends on the host mirrors any more (candidates dial the network
  aliases; fixture markers have in-process fallbacks).

## Grid log (sdb-queue, omp, high effort)

- **ling-3.1-flash** (spec tickets 0-8, 2026-10-15 run 1,
  `campaign_sdb-queue_openrouter_inclusionai_ling-3.1-flash_20261015_124730`,
  RULES `2026-10-15.3`, $0.80 cap, ~30 min): **reached 8 — the FULL ladder
  (9/9 tickets, 9/9 stages, zero regressions, zero repair rounds)**,
  $0.2412, tickets/$ 37.3. Every ticket passed on the first attempt:
  s0 genesis 24t ($0.0300) · s1 leases+ownership 18t ($0.0241) · s2
  nack/delays 18t ($0.0442) · s3 DLQ 18t ($0.0189) · **s4 durability
  (prepare -> SIGKILL -> restart -> verify) 18t ($0.0345)** · s5
  concurrency (6 consumers / 200 jobs) 12t ($0.0296) · s6 STATS 12t
  ($0.0205) · s7 delays+redrive 12t ($0.0160) · s8 hardening under the
  128MB cap 18t ($0.0234). This is the benchmark's **first complete
  survival curve** and the first time the destructive durability stage
  ever ran against a model candidate — and the first sdb-queue cell ever.
  Cross-app read: the same model reached 4/3-honest on sdb-edge and 4 on
  sdb-kv (both ladders wall earlier), so the queue ladder is materially
  easier for it — small line protocol, no RESP/HTTP semantics — which is
  exactly the per-app difficulty spread the portfolio exists to measure.
- Next for this campaign: more models (free tier) for spread, and the
  deferred depth tickets once a second model saturates (the ladder is
  complete for ling, so cost/survival comparisons need other samples).

## Grid log (sdb-kv, omp, high effort)

- **nemotron-3-super-120b-a12b:free** (spec 0-10, 2026-10-15 run 1,
  `..._043542`, RULES `2026-10-14.13`): reached **none** — s0 **FAIL
  2/3** ($0.0312, 28 initial + 12 repair = 40 turns), 0 regression-free.
  Two failure layers: (1) its C server's `ht_get` returns `kv_t*` but
  every caller treats it as `char*` -> segfault under the verifier
  (stages 1-2 pass, 3-4 time out after the crash, no pubsub; advisory
  102/103 die on connection drops); (2) **it could not operate the
  agent's bash tool** — after one successful call, every later `bash`
  invocation was rejected ("Service mode does not accept async or
  timeout; use ready.timeout for readiness") and it burned most turns
  guessing argument shapes instead of testing its code. Genuine model
  failure: harness-tool operation is part of the measured skill; the
  tool behaved per spec (early calls worked). Scheduled-cell mishap
  before this run: the 00:05 timer shell was GC'd overnight; relaunched
  manually at 04:35. **The 3-model grid now has cells for all three
  models** (ling best: reached 4).
- **ling-3.1-flash** (spec 0-10, 2026-10-14 run 12, `..._181711`, RULES
  `2026-10-14.13`): reached **0**. s0 PASS 3/3 ($0.0539, 28t, C). s1
  **FAIL 0/4** ($0.0607, 13+12, regressions [1,2,4] sev 3): the model
  rewrote `server.c` to add SET-options + pub/sub but dropped the RESP
  parsing section in the rewrite (build: undefined `parse_command`); the
  repair's reinsertion landed mid-`dispatch` (split function) and its
  turn budget went to edit-tool syntax friction trying to cut/paste the
  block; final build broken -> all stages refused. **Compliance fix
  verified live** (no false DQ; the candidate used a `/dev/tcp` probe,
  and client-mode nc/socat are now allowed by construction). $0.1145,
  tickets/$ 8.7.
- **ling-3.1-flash** (spec 0-10, 2026-10-14 run 11, `..._180418`, RULES
  `2026-10-14.12`): s0 PASS 3/3 ($0.0283, 19t, Python). s1 **invalidated
  by an anti-cheat false positive**: the model's own test script piped a
  RESP payload into `nc -q1 127.0.0.1 6379`, and the scanner flagged
  `nc` in any mode -> instant DQ (0/4). Client-mode nc cannot implement
  or tunnel a service; the cheat vector is LISTEN/EXEC mode. Fixed same
  day (RULES `2026-10-14.13`): only listen/exec forms fail, client-mode
  is allowed; tests cover both. Do not quote this cell; re-run replaces
  it.
- **ling-3.1-flash** (spec 0-10, 2026-10-14 run 10, `..._172751`, RULES
  `2026-10-14.12`): **reached 4 — new best; first run past s3.** s0 3/3
  ($0.0265, 28t), s1 4/4 ($0.0260, 13t), s2 5/5 ($0.0484, 13+12 repair),
  s3 durability **PASS 6/6 ($0.0675, 36t — the full pool)**: the Q2 raise
  (30->36) was decisive (four prior 30-turn attempts died in this class).
  s4 ENOSPC **PASS 7/7 ($0.0408, 30t)**: **stage 8's pass path executed
  live for the first time** — the fault container ran and the model's
  `SDB_DATA_DIR` + honest write-error implementation passed. s5 INFO
  **FAIL 0/8** ($0.0486, 13+12, regressions [1..6,8] sev 3): the initial
  attempt correctly failed stage 7 (`-ERR unknown command 'INFO'` —
  **stage 7's first live execution**, verdict machinery correct); the
  repair round botched the build (mangled `del`, unused imports, missing
  return) and ran out; re-verification refused. $0.2578, tickets/$ 19.4,
  5 regression-free. Harness clean (anchor, refs, ENOSPC lifecycle).
- **LADDER COMPLETE (2026-10-14 evening):** all 11 tickets built —
  stages 1-13. Ticket 10 (3-node HA, RULES `2026-10-14.12`) per operator
  decision Q1: automatic failover (T=10s, deterministic leader), rejoin
  pointed at the new leader, accumulated RESP suite + ./test.sh against
  the current master; partition safety deferred. Validation: `ha_node_args`
  unit test + ROLE/probe helper mock tests; live execution needs a model
  that passes t9 (none has yet). Spec total: 11 tickets, \$1.55 sum.
- **ling-3.1-flash** (spec 0-9, 2026-10-14 run 9, `..._170557`, RULES
  `2026-10-14.10`): reached **0** — first cell to fall at s1. s0 3/3
  ($0.0395, 28t), s1 expiry **FAIL 3/4** ($0.0430, 13 initial + 12
  repair). C this time. s1's initial attempt left `SET ... PX` as an
  unknown arity (`argc != 3`) and `entry_expired` defined but never
  called; the repair round diagnosed both correctly, started the pub/sub
  and expiry edits, then burned turns on edit-tool line-anchor churn
  (struct redefinition rejected twice) and hit the cap with none of the
  fixes landed; re-verification unchanged (3/4). $0.0826, tickets/$ 12.1,
  1 regression-free. Harness clean.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 8, `..._162654`): reached
  **2** — s0 3/3 ($0.0316, 28/28 — used the full new initial cap), s1 4/4
  ($0.0319, 13/13), s2 5/5 ($0.0276, **13/13 initial, no repair needed** —
  the trimmed initial is not starving the smaller tickets), s3 durability
  **FAIL 0/6** ($0.0494, 18 + 12, regressions [1..5]). $0.1405,
  tickets/$ 21.4. Go again. Initial attempt died mid-refactor (mangled
  edit -> syntax errors); the repair reconstructed `GetEx`, made `Persist`
  WAL-durable, fixed two call sites, and hit the cap exactly as its first
  `go build` ran (5 call-site mismatches left: Del/Append/ExpireAt/
  ExpireAfter now return two values). One more build-fix cycle would have
  landed it. **s3 pattern across runs 4/5/7/8: four consecutive failures,
  all "repair ran out mid-fix" on large Go refactors (~36+ turns needed
  vs the 30-turn pool).** Feeding the D6/pool calibration; nemotron cell
  tonight is the second model's evidence.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 7, `..._160300`): reached
  **2** — s0 3/3 ($0.0219, 20t), s1 4/4 ($0.0302, 13t at the reduced cap),
  s2 5/5 ($0.0455, 13 initial + 12 repair — the repair converted the
  pub/sub routing bug), s3 durability **FAIL 0/6** ($0.0441, 18 + 12,
  regressions [1..5]). $0.1417, tickets/$ 21.2, 3 regression-free. s3:
  the initial attempt left `NameError: deque` (client threads died before
  reading — every stage timed out; one smoke test would have caught it,
  none was run); the repair fixed that, discovered writes still failed
  (`main()` never called `wal_recover()`, so `WAL_FD is None`), diagnosed
  it correctly **on its final turn** and the fix edit emitted `def main():`
  twice -> IndentationError -> refused. Third straight cell where the
  repair lands a broken edit at the cap. Genuine; harness clean. Ling's
  effective profile on s3: needs ~35+ turns for the WAL ticket and always
  leaves 1-2 small bugs at the pool edge.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 6, `..._154729`): reached
  **1** — s0 3/3 ($0.0281, 18t), s1 4/4 ($0.0260, 13t — first live use of
  the 12-turn reserve, passed at the reduced initial cap), s2 pub/sub
  **FAIL 0/5** ($0.0368, 13 initial + 12 repair, regressions [1,2,3,4]
  sev 3). $0.0909, tickets/$ 22.0, 2 regression-free. Python again. The
  initial attempt was close (4/5): the pub/sub handlers existed but
  `dispatch` never routed SUBSCRIBE/UNSUBSCRIBE/PUBLISH. The repair round
  diagnosed it correctly, then fumbled the edit tool — `PUT 476*:` was a
  whole-function block replace (it meant the signature line), and a second
  operation line inside the same edit became literal text (`PUT 522>`),
  destroying the dispatch body; the last turns went to fighting the damage
  and the re-verification hit a SyntaxError -> 0/5 -> regressions
  [1,2,3,4]. A genuine "repair made it worse" cell: exactly the cardinal
  sin the regression-free metric is built to catch. Harness clean.
- **Harness validation — eval-only replay** (2026-10-14, `..._151415`, no
  model calls): seeded the run-3 workspace through the full 0-6 spec.
  t0-t3 **PASS** (3/3, 4/4, 5/5, 6/6 — the AOF SIGKILL/restart is clean on
  the fixed binary), t4 **FAIL 6/7**: **stage 8 executed live for the
  first time** — the fault container (8MB tmpfs `/data`, `SDB_DATA_DIR`)
  started, served, and swallowed the verifier's 240KB fill with a hang.
  Reproduced standalone on a workspace copy: the candidate wedges
  permanently on any `SET` >= 64KB (60s+ no reply; 1KB works), i.e., a
  genuine candidate bug caught by the ticket, not a verifier timeout.
  All three advisory probes **PASS** (slow-sender self-seed fix,
  differential vs ref-redis, malformed survival); anchors keep every
  stage network alive. Stage 7 (INFO) and stage 9 (tests&CI) remain
  unexecuted live; canary run 5 in flight chases them.
  *(2026-10-14 follow-up: writing a counting-mock test for stage 7 found
  its `total_connections_received` delta check unpassable — the test
  never opened a second connection, so a faithful server's correctly flat
  counter failed. Fixed: the test now connects/disconnects a second
  client; mock test asserts pass + frozen-counter fail. RULES
  `2026-10-14.7`. Stage 7 still needs a live execution.)*
  *(2026-10-14 follow-up 2: stage 9's revert-sensitivity flow now has an
  end-to-end test against a real fixture git repo — green suite passes,
  toothless always-green suite is rejected. Stage 9 needs a live campaign
  run only if the ladder ever reaches t6.)*
- **Ticket 7 (dashboard) built** (2026-10-14 late, RULES `2026-10-14.8`):
  stage 10 verifier (offline HTML + `/api/stats` deltas + SSE JSON frame
  under concurrent RESP traffic), worker-8090 port publishing gated on
  acceptance id 10, prompt + spec ticket 7. **Validation: verifier logic
  is mock-tested (pass / starved-SSE / frozen-stats); the port extras have
  an args unit test. Live plumbing validation (candidate container with
  the published port + the stage-10 block) still needs a run that reaches
  t7 — no model has passed t3 yet.** Nemotron cell at 2026-10-15 00:05
  runs the 9-ticket spec; its path through t0-t6 is behavior-identical to
  earlier cells (stages 10/11 gate on acceptance only).
- **Ticket 8 (security) built** (2026-10-14 late, RULES `2026-10-14.9`):
  stage 11 verifier — RESP auth gate (`SDB_PASSWORD`), dashboard Basic
  auth (401 bare), oversized-frame probes under a 64MB memory cap
  (64MB bulk header / 2M-arg multibulk must react without buffering), an
  8-payload fuzz corpus with liveness after each, a 12-connection
  slow-loris probe, and a 2000-line container-log scan for the secret.
  Pipeline restarts the candidate with the secret env + `64m` cap and
  restores it afterwards. **Validation: threaded mock test (faithful
  pass / skipped-auth fail / buffered-oversize fail); live plumbing
  awaits a t8-reaching run.**
- **Ticket 9 (perf) built** (2026-10-14 late, RULES `2026-10-14.10`):
  stage 12 — pipelining in order (200 commands in one write, positional
  checks; mock test caught the coalesced-reply reader bug and now covers
  pass + wrong-reply fail), 300-GET sequential p99 budget (10ms), and a
  post-bench gate requiring the stage's own bench measurement to reach
  >= 1.3x the genesis-stage value (carried via `baseline.json`; pure
  `perf_gate_verdict` unit-tested). Measurement study: 5 consecutive
  same-code runs on an idle machine spread 21.6k-22.0k rps (~1%), well
  under the 30% gate; within-campaign stage-to-stage spread for the same
  code was ~5% — the earlier 55k->31k swings were implementation changes
  plus concurrent host builds. **The prompt cannot tell the model its
  exact baseline number (static prompt files); models optimize blind —
  acceptable for v1, noted below.**
- **Open calibration question (for the user)**: ling has failed s3 four
  consecutive times in the "repair ran out mid-refactor" class (runs
  4/5/7/8; needs ~36+ turns vs the 30-turn pool). Options: raise s3's
  pool to 36, raise the repair floor to 16, or keep it as the
  discriminator. Nemotron's cell tonight is the second model's evidence.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 4, `..._144606`): reached
  **2** — s0 3/3 ($0.0400, 30t), s1 4/4 ($0.0496, 17t), s2 5/5 ($0.0424,
  25t), s3 durability **FAIL 5/6** ($0.0610, 22 initial + 8 repair = 30t).
  $0.1930 total, tickets/$ 15.5, 3 regression-free. **Genuine model
  failure, harness fixes held** (no network murder; the AOF restart
  mechanics worked). This run the model rewrote in Go; the initial session
  never compiled (`main.go` missing `package main`), the 8-turn repair
  fixed both compile errors (package + duplicate `store.del`) and hit the
  limit before testing. Root cause of the AOF loss (reproduced manually on
  a workspace copy): `delFields` uses `make([]byte, 4, ...)` — slice
  LENGTH 4, not cap — so every DEL record carries 4 junk zero bytes before
  the count; replay reads `count=0`, trips the leftover-bytes check, and
  truncates every record after the first DEL (the probe). Classic Go
  slice-length bug; exactly what ticket 3 is for. Advisory 102 failed on
  the pre-`5cf5a86` binary (self-seed fix landed after launch).
  **Calibration signal #2**: the 8-turn repair floor could not recover a
  500-line implementation that never compiled (ling needed ~2-4 more
  turns to test durability; inkling's probe never edited at all).
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 5, `..._152002`): reached
  **2** — s0 3/3 ($0.0263, 30t), s1 4/4 ($0.0356, 17t), s2 5/5 ($0.0338,
  17t), s3 durability **FAIL 0/6** ($0.0556, 22 initial + 8 repair,
  regressions [1,2,3,4,5] sev 3). $0.1514 total, tickets/$ 19.8, 3
  regression-free. Go again. The initial attempt ended mid-refactor
  (`walLog` undefined, unused imports, never built); the 8-turn repair
  spent 7 turns surveying a 900-line file and its single edit landed at
  the limit — and duplicated a `"sort"` import, so the re-verification
  did not compile either. Harness clean (refused-connection verdicts are
  the candidate's failure). **Calibration signal #3** — see the D6
  amendment below: floor 8 -> 12.
- **ling-3.1-flash** (spec 0-5, 2026-10-14 run 3, `..._140445`): s0-s3 PASS
  3/3/4/5/6 (30t/17t/25t/22t, $0.0286/$0.0333/$0.0369/$0.0752), then s4
  **invalidated by a harness murder**: while the AOF probe had the candidate
  SIGKILLed (2s window), the engine's 60s periodic `prune_labeled_networks`
  sweep — which cannot see foreground CLI campaigns (its busy check only
  covers engine worker_runs) — saw the 354s-old stage network with zero
  *running* endpoints and removed it; `docker start` then failed with
  "network ... not found", stage 8 was skipped, and the repair re-verification
  added a second false regression (stage 4 INCR :2 from WAL state carryover).
  Rollup says reached 3 / regressions [4,6] but both are harness artifacts.
  Fixes: run anchor container (network never looks empty; see `ensure_run_anchor`)
  + stage-4 DEL-first (RULES 2026-10-14.5). **Rerun in flight** to replace
  this cell; keep the dir as incident evidence, do not quote its numbers.
- **ling-3.1-flash** (spec 0-3, D6b): reached **3** tickets — s0 3/3 (30t),
  s1 4/4 (19t), s2 5/5 (19t), s3 6/6 (23t + 7 repair). 4/4 regression-free,
  $0.1743, tickets/$ 22.9. Repair converted a 3-line compile error.
- **inkling:free** (spec 0-3): reached **1** — s0 3/3 (14t, $0.0112),
  s1 4/4 (19t, $0.0192), s2 FAIL 4/5 ($0.0117, 25t: 19 initial + 6 repair).
  Initial attempt hit its cap mid-pubsub; the 6-turn repair spent all six
  turns reading/surveying and never made an edit. $0.0421 total,
  tickets/$ 47.5. **Calibration signal**: the max/4 repair reserve (min 1)
  can be too small for slower models — consider a floor (e.g. 8 turns) or
  larger stage budgets once more cells confirm; do not tune on n=1.
  *(Resolved later same day: floor raised to 12 — see the D6 amendment.)*
- **inkling:free (probe, floor=8)**: reached **0** — s0 PASS 3/3 (30t,
  $0.0156), s1 expiry FAIL 3/4 (17 initial + 8 repair, $0.0071). The
  repair round again read/surveyed for its entire budget and NEVER edited
  (last actions: `head -c 100 server | xxd` -> missing tool -> cap). The
  binding constraint for this model is repair-round *behavior* (it does
  not switch to editing under budget), not the reserve size: 6 turns
  (run A) and 8 turns (run B) both failed the same way, and run-to-run
  variance is large (s0 took 14 turns in A, 30 in B). Floor was kept at 8
  for this cell *(superseded 2026-10-14: raised to 12 after three cells
  showed repair-starved refactors; see the D6 amendment)*; do NOT tune
  further on n=1; collect more cells. $0.0227, tickets/$ 44.1.
- **nemotron:free**: quota-blocked (`free-models-per-day-high-balance`,
  retry ~10.4h). Recorded as a ticket-0 **FAULT** (1 turn, $0.0002) — the
  throttle classification worked; acceptance counted 3 stages, campaign
  stopped. Two refinements landed from this cell: the repair loop is now
  SKIPPED when there are no stage results (no runnable candidate -> a
  repair round is meaningless; the faulted cell burned one against an
  empty workspace), and the rollup says "stopped: ticket 0 fault" instead
  of "ticket 0 failed". Re-run next day.

## Open decisions (operator)

- [ ] D1. HA failover: automatic election (spec as written) vs manual
  promote command.
- [ ] D2. Partition/split-brain: in v1 or a later incident stage.
- [ ] D3. Curve choice (G3).
- [ ] D4. SPRT vs top-up (G2).
- [ ] D5. Repo CI coverage gate 50 -> 80 (`--fail-under-lines`).
- [x] D6. Stage budgets (2026-10-14): option **(b)** implemented — campaign
  stages reserve `max_turns/4` (min 1 turn) for the repair round; initial
  cap = min(max_turns - reserve, 50). Single-shot behavior unchanged; big
  pools unaffected (the 50-cap binds first). See DECISIONS #12. Canary
  re-run under the policy (watch: does ticket 1 convert with repair room?).

## Rules & gates

- Campaign spec + stage prompts + suites join GRADED_FILES -> new stamp
  per campaign version; freeze specs during a wave.
- Flake bar: new stage test passes 20/20 before entering a campaign;
  injectors idempotent + reversible; `test.sh` deterministic.
- No board mixing (single-shot vs campaign); no retroactive rescoring.
- Engine idle during deploys; migrate-check clean after.

## Hygiene backlog

- [x] Incident fix (2026-10-14): a SIGTERM-killed campaign leaked its
  reference container holding host port 6380 for 2h; `start_reference_redis`
  swallowed docker-run failures (`let _ = …; Ok(())`), so every later
  campaign silently measured against a missing ref ("Could not connect to
  ref-redis") with no warning. Fixed: both reference starters return Err
  with stderr on failure, and the campaign CLI wires SIGINT/SIGTERM to the
  pipeline cancel flag so cleanup guards reap running containers. After a
  hard kill, `prune --yes` remains the belt.
- [x] Incident fix (2026-10-14, engine-sweep murder): the engine's 60s
  periodic reconcile runs `prune_labeled_networks` while a foreground CLI
  campaign is live (the busy check only sees engine-owned worker runs), and
  a stopped candidate counts as zero endpoints — so a >300s-old stage net
  whose candidate is in its AOF kill window was removed mid-stage (s4
  "network ... not found"). Structural fix: every run network now carries a
  `subdollar-anchor-*` container (`ensure_run_anchor`) that keeps the
  network non-empty for the run's whole intended life; zombie anchors are
  reaped by the orphan sweep's 3h age gate. Side effect: fixes the
  equivalent race for any future sweeper, ours or not.
- [x] Cleanup (2026-10-14): pre-purge recovery branch + `refs/original`
  dropped, stale stash dropped, reflog expired, gc pruned the leaked-key
  blobs (verified absent). Stale root logs removed; data archived + reset
  (see TODO_EFFORT §S).
- [ ] README: campaigns section once Slice A lands.
- [ ] DECISIONS.md: one entry per resolved open decision.

## Scope guards

- Do not start campaign #2 before sdb-kv canary + one grid pass.
- No new task types until the campaign machinery proves.
- No curve/scoring changes without an anchored wave.
