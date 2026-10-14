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

## Grid log (sdb-kv, omp, high effort)

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
- **inkling:free (probe, floor=8)**: reached **0** — s0 PASS 3/3 (30t,
  $0.0156), s1 expiry FAIL 3/4 (17 initial + 8 repair, $0.0071). The
  repair round again read/surveyed for its entire budget and NEVER edited
  (last actions: `head -c 100 server | xxd` -> missing tool -> cap). The
  binding constraint for this model is repair-round *behavior* (it does
  not switch to editing under budget), not the reserve size: 6 turns
  (run A) and 8 turns (run B) both failed the same way, and run-to-run
  variance is large (s0 took 14 turns in A, 30 in B). Keep floor=8 for
  now (it only touches pools <32: 25->17+8), do NOT tune further on n=1;
  collect more cells. $0.0227, tickets/$ 44.1.
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
