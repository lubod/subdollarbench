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

## Slice B — stage suites (expiry, pubsub)

- [ ] B1. Expiry suite: TTL family semantics (SET clears TTL, INCR
  preserves, negative/zero deletes, PTTL rounding, GETEX options).
- [ ] B2. Pub/sub suite: seeded channels, multi-channel subscribe,
  unsubscribe silence, publish ordering, receiver counts.
- [ ] B3. Stage-tag the existing redis verifier tests so cumulative
  selection is data, not code.

## Slice C — durability, incidents, observability

- [ ] C1. AOF acceptance: fsync-before-ack, SIGKILL + replay, torn
  tail, replay idempotence (reuse WAL verifier patterns).
- [ ] C2. ENOSPC injector: AOF dir on a small tmpfs; reads keep
  serving, writes return an error safely, auto-recovery when space
  frees, AOF never corrupted.
- [ ] C3. INFO suite: exact counters under randomized scripted traffic;
  structured error log check; secret-never-logged grep.
- [ ] C4. Parsers for INFO fields and log lines; counter-delta helper.

## Slice D — tests stage (coverage with teeth)

- [ ] D1. `./test.sh` contract: offline, deterministic (two runs), time
  cap, exit 0 = green.
- [ ] D2. Coverage adapters: Go (`go test -coverprofile`), Node
  (`--test --experimental-test-coverage`), Python (`coverage` in the
  image); other languages: self-report + sensitivity only (documented).
- [ ] D3. Sensitivity checker: scratch copy, sentinel reverts (stage 1
  expiry, stage 5 observability), require non-zero exit; compile failure
  counts as caught.
- [ ] D4. Persistent gate: from stage 6 on, `test.sh` green + >=80%
  coverage are part of cumulative acceptance (regression if dropped).
- [ ] D5. Sandbox image: add tooling (python coverage; optional
  cargo-llvm-cov); bump image version + document.

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

## Open decisions (operator)

- [ ] D1. HA failover: automatic election (spec as written) vs manual
  promote command.
- [ ] D2. Partition/split-brain: in v1 or a later incident stage.
- [ ] D3. Curve choice (G3).
- [ ] D4. SPRT vs top-up (G2).
- [ ] D5. Repo CI coverage gate 50 -> 80 (`--fail-under-lines`).
- [ ] D6. Stage budgets: canary finding — `max_turns` is the shared pool
  (initial cap = min(max_turns, 50) when feedback is on), so ticket 1's
  25 turns were consumed by the initial attempt and the repair round
  never ran. Options: (a) budget stages >=60 turns so the 50-cap leaves
  headroom, (b) campaign-side reserve (initial = min(max_turns - max/4,
  50)), (c) accept cap-exhaustion as a fail. Calibrate on the re-run.

## Rules & gates

- Campaign spec + stage prompts + suites join GRADED_FILES -> new stamp
  per campaign version; freeze specs during a wave.
- Flake bar: new stage test passes 20/20 before entering a campaign;
  injectors idempotent + reversible; `test.sh` deterministic.
- No board mixing (single-shot vs campaign); no retroactive rescoring.
- Engine idle during deploys; migrate-check clean after.

## Hygiene backlog

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
