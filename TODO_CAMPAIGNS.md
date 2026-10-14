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

## Grid log (sdb-kv, omp, high effort)

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
