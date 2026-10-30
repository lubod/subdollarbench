# Deferred decisions

One entry each; 5 lines max. Status + next trigger, not essays.

## 1. Throughput curve deliberately unchanged (2026-10-03.5, updated 2026-10-14)
`src/report/rules.rs` regime notes: verifier pins shipped, curve untouched —
zero anchored ratios in the fresh wave (5/5 drifted), so any retune is unverifiable.
2026-10-14: reproducibility evidence arrived by the batch — the same pinned ref
measured 87.7k vs 142.2k within seconds (two concurrent batches), tracking the
n=5000 window's 2.31x spread/CV 27.5%; n=100k windows measure 1.26x/CV 5.8%
(10 windows, same client/path). Fix landed: windows lengthened + bench files
hashed (RULES 2026-10-14). Curve still unchanged — now it has a stable
instrument to validate against.
Revisit when a wave yields anchored redis/http ratios under the 2026-10-14 stamp.
Ref: `6891313`, `src/report/rules.rs`, `src/bench/mod.rs`.

## 2. DNS task removed, rows archived (2026-10-05)
Lenient verifier (hard-coded-table satisfiable) + unmeasurable mock denominator
+ unprovable `.invalid` forwarding. Code+prompt+verifier deleted; 5 rows in
`provenance-witness/dns-archive-20261005.tar.gz`; strings still parse.
Revisit only with a strict verifier + real CoreDNS denominator. Ref: `ce08325`.

## 3. Reasoning effort is a request, not a guarantee (2026-10-01)
n=1/cell probe: deepseek ignored the flag (13 turns both settings); qwen +54%
completion while totals fell. Not a controlled variable — waves run ONE level.
Revisit with n>=9/cell completion-token coefficients. Ref: `1a06967`, README §effort.

## 4. Egress proxy is not an enforcement boundary
Allowlist sidecar = `*_PROXY` env vars; raw sockets bypass it by design
(measured 2026-09-30). `--internal` net + abort-on-sidecar-fail is the real
containment; per-run capped keys bound the blast radius. Ref: `9868226`, `src/sandbox/omp.rs:490-525`.

## 5. Compute floor inside effective cost (2026-10-03)
`C_eff = C_API + T*$0.00003 + N*$0.00000005` (cached 0.25x): wall-clock proxy so
idle-spin isn't free; dominates cheap-model cost by design. Cached fraction cut
0.25 after double-billing finding (109k cached of 127k on one run).
Revisit if latency variance swamps model signal. Ref: `src/cost/calculator.rs:18-35`.

## 6. Review judge defaults to muse-spark, advisory-only (2026-10-03.7)
`review_model` defaults to `openrouter/meta/muse-spark-1.3-contributor`
(`src/config.rs:127,150`); synthesis is grounded in deterministic stats +
manifest failures, never a verdict. `canonical_best` is the shared tie-break
(pass rate, then throughput else efficiency). Revisit on judge-model rotation.

## 7. Audit repair: cures, consolidation, DQ exclusion (2026-10-09)
Second-agent audit verified 5/7 findings; fixed all: compliance test-scope
+ import-line skip, launch-error split, 100%-pass status parity, `repair`
command (7 DQ + 41 agent_killed cures, 12 loose groups merged, 818 runs
pruned), ranked boards exclude cheats. Declined: CSRF lockdown on auth
bootstrap (breaks headless CLI), zero-cost rescoring (policy, not defect).
Stamp `2026-10-09.1`; prior stamps need `backfill --yes` to unify.

## 8. Zero live-delta is not zero spend (2026-10-09.2)
Cost came from `live_spend_delta.unwrap_or(formula)`. `/auth/key` reports
*weekly* usage with variable lag, so a counter that had not caught up read
exactly `0.00` — and 144 archived muse-spark runs (2026-10-06, a paid model
at $0.10/$0.20 per M) recorded that false zero. It flattered their
efficiency scores and parked a paid model on the free board. Rule now:
a delta that *moves* is the measurement (credits, promos, provider-specific
billing live there); a zero delta never overrides a positive formula cost —
the token-priced formula wins and the disagreement is recorded
(`resolve_api_cost`, sources `live` / `formula` / `formula_zero_delta`).
Retroactive fix is **annotation, not rescoring**: `cost-annotate` writes
`cost_formula_usd` from recorded tokens at *catalog-confirmed* prices onto
those rows (manifests, results, both bench.db tables), so `tierOf` reads
them paid. Archived `cost_usd`, effective cost, and scores are untouched —
re-deriving published numbers is a grading decision, needs its own review.
An unconfirmable price (catalog miss) leaves the row alone rather than
pricing it from the offline fallback, which would relabel genuinely free
rows as paid. Ref: `src/cost/calculator.rs`, `src/report/cost_annotate.rs`.

## 9. Machine effort on both harnesses + recovery over reclassification (2026-10-09.3)
Effort is harness-relative by construction (`max` = most the path
supports; ranks never mix harnesses). OMP drives it through the
`--thinking` machine flag; its runner-side max→high pre-clamp was removed
after live probes proved the CLI downgrades unsupported levels itself
(`thinkingLevel: "high"` recorded, exit 0, no HTTP 400) — the downgrade is
now surfaced as `[THINKING]` next to the requested level instead of
silently pre-applied. OpenCode maps effort to model `#variant`
(`-m provider/model#variant`, per-trial `OPENCODE_CONFIG_CONTENT`
definitions), which the openrouter package forwards as native
`reasoning.effort`; prose stays as fallback, and a variant-resolution
failure retries once prose-only (instant, zero-spend, bounded by flag).
Turn-1 reasoning cutoffs (exit 0 + 0 tools + empty workspace) resume the
same session with a tool-first nudge, cap 2 per trial — recovery converts
dead trials into scored ones, while retroactive reclassification of the 26
archived cutoff rows was rejected (verdict-honesty rule: the model
disobeyed explicit tool/concision directives, so the 0% stands; history
gets annotated, never rescored). Turn-telemetry fix (steps_taken
propagation, 171/171 opencode rows read 0) rides along with no grading
impact. Ref: `src/sandbox/opencode.rs`, `src/sandbox/omp.rs`,
`TODO_EFFORT.md`.

## 10. Proxy fixtures go deterministic (2026-10-11.2)
Stages 1–2 learn upstream markers from identical in-process stubs when
host publishes decay (fallback, docker path untouched when healthy);
dead containers fail loud naming the fixture instead of misattributing.
Stage 4 kills via pause/unpause (immune to stop/start re-publish and
concurrent-rm races) with rejoin proven via proxy rather than direct
dial — strictly stronger (slow rejoin now fails) and documented as such.
Bumped (not silent) because stage-4 verdicts can differ: consistency with
the file's every-change-bumps history outweighs the "same wire behavior"
argument, and the strictness delta deserves visibility. Ref:
`src/verifier/proxy.rs`, `TODO_EFFORT.md §M`.

## 11. Campaign portfolio: compose capability stages, don't run 7 campaigns (2026-10-14)
Seven single-shot tasks do not become seven campaigns: verifier rigor
(seeded, byte-level, anti-cheat hardened) is the expensive asset and
campaigns compose it — sdb-kv absorbs redis/pubsub/wal (tickets 1-3) plus
later disk/compaction + admin binary protocol; sdb-edge <- proxy;
sdb-http <- http; **sdb-queue** is the one new app (leases/ack/nack/DLQ
= temporal state machines); pdf deprioritized to archive. Single-shot
boards retire to genesis/history within their stamps; nothing is deleted
(archive/annotate; DNS is the only removal precedent, for an unsound
verifier).
Next trigger: after the sdb-kv model grid validates the method; port
order sdb-kv stage-2 -> sdb-edge -> sdb-queue -> sdb-http.
Ref: `TODO_CAMPAIGNS.md` portfolio, `docs/PROPOSAL-iterative-campaigns.md`.

## 12. Campaign repair reserve: initial cap = min(max_turns - max_turns/4, 50) (2026-10-14)
Live canary evidence: a 25-turn ticket consumed the whole shared pool
(25/25) on the initial attempt, the repair round never fired
("Global turn budget reached across attempts"), and a last-minute rewrite
shipped a compile error that repair would have caught. `max_turns` IS the
pool; the old initial cap = min(max_turns, 50) left a 25-turn ticket with
zero repair room. Option (b): campaign stages reserve a quarter of the
pool (min 1 turn) for repair; single-shot behavior unchanged; pools >=50
are unaffected (the 50-cap binds first). Revisit if models thrash against
the reduced initial budget (watch cutoffs / regressions on the re-run).
Ref: `src/pipeline.rs` initial_turn_cap, `TODO_CAMPAIGNS.md` D6.
Validated live (2026-10-14, run 2): all four canary tickets passed;
ticket 3's initial attempt hit its cap mid-WAL with a three-line compile
error, and the 7-turn repair round fixed exactly those lines to 6/6 —
the failure mode that motivated the reserve.

**Amendment (2026-10-14, same day): floor 8 -> 12.** Three further cells
showed the 8-turn floor cannot cover the dominant failure class — an
interrupted refactor left uncompilable code at the initial cap:

- ling run 4 (Go): repair fixed `package main` + duplicate `store.del`,
  then hit the limit immediately; 2-4 more turns would have reached a
  build. (The residual AOF replay bug then failed the ticket honestly.)
- ling run 5 (Go): repair spent 7 turns surveying a 900-line file and its
  single edit landed at turn 8.
- inkling probe: never reached a single edit inside 8.

Meanwhile repairs that ARE small (a deterministic pub/sub bug, ling run 4
s2) succeed inside 8 and will keep succeeding. So reserve =
`max(max_turns/4, 12)`, capped at `max_turns/2`; small pools pay up to 4
turns of initial budget (25->13, 30->18, 40->28), which is net-positive
for the observed class: models that leave uncompilable code do not finish
at 17-30 initial turns either, while repair starts from a precise failure
report on a near-complete file. Graceful degradation for tiny pools is
unchanged (10->5, 4->2, 1->1). Revisit if small tickets start failing
initially where they used to pass.

## 13. Operator decisions (2026-10-14): HA, s3 pool, protocol gating, linear curve, CI gate (2026-10-14)

Six open questions answered by the operator (full detail:
`docs/OPEN-QUESTIONS.md`); all landed the same day under RULES
`2026-10-14.11` unless noted:

- **HA finale (ticket 10)**: automatic failover — survivors promote the
  replica with the highest replication offset (tie: lowest port) within
  **T = 10s**; the network-partition stretch check is deferred. The build
  follows the proposal's Stage 10 launch/replication/sync-ack/rejoin
  contract.
- **s3 durability pool 30 -> 36 turns** (initial ~24 + 12 repair):
  four consecutive ling cells died at the 30-turn wall in the same
  "repair ran out mid-refactor" class. Cells measured earlier used 30
  (loose comparability; grid log notes it per cell).
- **Protocol gating (scoring-v2 option A)**: `validate_comparison_wave`
  refuses mixed-protocol waves (legacy stamps group as `unknown`);
  `merge_subruns` stamps mixed parents `mixed`; portal rollups group and
  badge by (rules regime, protocol). SPRT (C) and top-up (B) deferred.
- **Throughput pricing is linear**: `score = base_eff x speed_ratio`
  (retires `r(1+2r)`, where 2x speed could cost 10x and tie). Revisit if
  a wave shows r > 1.5 is a reliable quality signal.
- **Bench CI coverage gate 50 -> 80** (`--fail-under-lines 80`),
  dogfooding the campaign's own §6 standard. Actions stays disabled, so
  the gate is inert until CI is re-enabled; the bench measured ~70% lines
  and needs coverage work on docker-gated paths first (TODO_CAMPAIGNS
  hygiene).

## 14. Stage-2 wave: disk residency, compaction, SDB1 v2 admin protocol (2026-10-15.1)

The sdb-kv ladder grows three tickets (11-13; pipeline stages 14-16;
RULES `2026-10-15.1`; acceptance ids up to 16). Design choices and why:

- **Disk residency is enforced by the container memory cap, not by
  inspecting the implementation.** Stage 14 restarts the candidate with
  `SDB_DATA_DIR=/data` on a host bind mount under a 192MB limit, writes
  ~480MB of 240KiB values, reads back byte-exact, deletes, then SIGKILLs
  and restarts. A value-per-key RAM map OOMs; an offset-index +
  append-log passes. We grade the observable property (serve data far
  larger than the process limit, survive a crash), not a design.
- **The compaction gate measures the host bind mount, not self-reported
  numbers** (<= 3x live bytes within 30s after ~860MB of churn; 48MiB
  live => <= 144MiB). 3x tolerates rewrite/AOF amplification while still
  proving real reclamation. Values are hex-of-PRNG: poorly compressible,
  so zipping values cannot fake the footprint; the bound is generous
  enough that legitimate per-value compression plus compaction still
  passes.
- **Admin protocol v2 = the existing SDB1 asset, extended in place**
  (proposal: "admin binary protocol v1 -> v2"). Framing is unchanged
  (magic/seq/opcode/status/len/CRC32); v2 adds STATS (0x06, a
  `field:value` payload mirroring the INFO counters) and requires one
  store behind both protocols (SDB1 PUT <-> RESP GET in both directions).
  Cross-protocol coherence is cheap to check and expensive to fake; the
  verifier reuses the binproto framing crate asset (98% covered).
- **Port 6395 is published only when a ticket's acceptance includes
  stage 16** (worker offset), same discipline as the dashboard port: an
  unused published port is a collision failure mode.
- **Ticket evolution, not a new campaign**: the stage-2 wave appends
  tickets to the living sdb-kv workspace so the ladder story stays one
  siteline; older cells are stamped earlier rules and remain
  non-comparable without `backfill --yes` (loose-comparability policy).
- Offline validation: mock SDB1+RESP servers (pass, split-store fail,
  STATS-field fail), sparse-log compaction pass + no-shrink fail,
  live-docker bind round-trip. Live execution of 14-16 awaits a model
  that reaches ticket 11.
- Live proof (same day): `test_pipeline_eval_only_stage2_disk_and_admin_live`
  drives a reference mock candidate (offset index + append log + idle
  compaction + SDB1 v2) through the real pipeline for stages 14-16; all
  three pass, including the SIGKILL re-reads and the footprint gate. Real
  campaigns on worker 0 publish 6395 1:1 as documented; eval-only workers
  hand the offset admin port to the candidate via argv.

## 15. sdb-edge: the proxy campaign (2026-10-15.2)

Second portfolio app (DECISIONS #11 order) and the first non-redis
campaign. Task `proxy`, seven tickets (0-6) on one living workspace,
RULES `2026-10-15.2`. Design choices:

- **Task adapter, not a fork**: `Acceptance` gained `proxy_stages`
  alongside `redis_stages`; validation ranges are per task (redis 1..=16,
  proxy 1..=9); the CLI and rollout resolve the list for the campaign's
  task. Adding an app is now a task adapter, not a new code path.
- **Single-shot boards are frozen**: stages 6-9 run only when a campaign
  acceptance includes them (`campaign_only`), so the existing 5-stage
  proxy task and its history are untouched; new grading lives in new
  stamps only.
- **The render score extends the asset we already had**: routing,
  round-robin, framing/keep-alive, failover and throughput came from the
  single-shot verifier (88% covered); the four new stages reuse its
  marker fixtures, `docker pause` failover pattern and mock harness.
- **Stage 6 vs stage 4**: stage 4 (pause + zero failures) accepts slow
  clients that eventually succeed through a client deadline; stage 6
  makes the *per-request* budget explicit (2500ms worst case) so a proxy
  without upstream timeouts/retry fails even though every request
  "eventually" returns 200.
- **Stage 7 grades a health surface, not just behavior**: `/status` with
  `upstreamN: up|down` lines, flipped by active probing **while the proxy
  is idle** (passive inference cannot fake it), and recovery <=5s.
- **Stage 8 is default-off** (`SDB_PROXY_CACHE_TTL=0`/unset = no
  caching): earlier tickets run without cache semantics, and the stage
  restarts the candidate with the TTL env, so caching can never mask
  round-robin or failover (a path-keyed cache would otherwise serve one
  upstream forever). `X-Cache`/`Age` make cache behavior observable, and
  byte-identical HIT bodies block header-only cheating.
- **Stage 9 mirrors the sdb-kv hardening ticket**: cheap rejection
  (431/413), slow-loris reclamation under a 128MB cap, liveness after.
- **Throughput (stage 5) stays single-shot** until proxy baselines are
  recorded (a perf ticket with the 1.3x gate is a later ticket, J6), and
  **TLS termination is deferred** to J6 (a verifier TLS client is needed;
  stdlib-only Rust candidates would otherwise be disadvantaged).
- Live proof (2026-10-15): the reference proxy passed the full ladder
  through the real pipeline (acceptance [1,2,3,4,6,7,8,9]; all 8 stages
  PASS). The run exposed one harness defect and fixed it: candidate
  restarts did not re-attach to the run network, so a restarted candidate
  could not resolve `upstreamN` aliases (symptom: getaddrinfo stalling
  against the host resolver). Restart blocks now call
  `connect_candidate_to_network()` (proxy stages 8/9 and the two redis
  disk stages).

## 16. sdb-queue: the job-broker campaign (2026-10-15.3)

The portfolio's one genuinely new application (DECISIONS #11). New task
type `queue` (port 6385), nine tickets (0-8), 9-stage suite, RULES
`2026-10-15.3`. Design choices:

- **Lease ownership by connection**: a lease belongs to the connection
  that took it; ACK/NACK from elsewhere is `-ERR not-owner`. This makes
  duplicate-ack and stolen-lease bugs observable without a separate
  receipt-handle concept, and it is what the concurrency stage leans on.
- **In-flight recovery is explicit**: on restart, leased-but-unacked jobs
  return to the queue with attempts preserved (the next lease reports
  attempts+1), acked jobs never return. This is the queue analogue of the
  redis AOF contract and the sharpest part of the durability ticket.
- **Dead letters are a queue, reads are inspections**: after 3 failed
  attempts a job moves to `<queue>.dead`; leasing from `.dead` does not
  increment attempts (the reported count is the failed-delivery count).
  Redrive (`REDRIVE <queue> <id>`) is the operator's resurrection path and
  resets attempts.
- **STATS is per queue and reads are side-effect free**: the counters must
  add up under a known workload (`enqueued/acked/nacked/inflight/
  available/dead`), which catches bookkeeping drift that behavioral stages
  would miss.
- **The hardening ticket mirrors the neighbours**: 32KiB payloads rejected
  against an 8KiB cap, malformed lines error without crashing, a stalled
  partial line is reclaimed within ~12s, all under a 128MB cap.
- **Campaign-first**: unlike redis/proxy/http there was no single-shot
  task to preserve, so the 9-stage suite is both the single-shot board and
  the campaign acceptance; `queue_stages` is the acceptance list and the
  validator range is 1..=9.
- **Mock-first rigor**: the tests' mock broker implements the full lease
  state machine, and eight injected defects each prove the stage that is
  supposed to catch them does, with a naming error message. The live
  proof (a reference broker through the real pipeline, including the two
  restart halves) is the next step (TODO_CAMPAIGNS Slice K6).
- Live proof (2026-10-15): the reference broker passed the full queue
  ladder through the real pipeline (acceptance [1..9]; all 9 stages PASS,
  ~20s), including the two restart halves. The proof immediately found a
  verifier defect: the durability *prepare* leased jobs assuming FIFO
  would hand it the job it had just enqueued, but a correct FIFO broker
  returns the earlier waiting job — the prepare now settles each state
  (ack -> dead -> in-flight -> waiting) before the next enqueue, and a
  mock test exercises both halves.
- First model cell (2026-10-15): ling-3.1-flash solved the complete queue
  ladder — reached 8, 9/9 tickets, zero regressions, zero repairs,
  $0.2412 (tickets/$ 37.3), including the destructive durability ticket on
  the first attempt. The benchmark's first complete survival curve; the
  queue ladder is materially easier for this model than redis/proxy
  (where it walls at t4), which is the per-app spread the portfolio
  exists to measure.

## 17. sdb-http: the file-server campaign (2026-10-15.4)

The final portfolio app (DECISIONS #11 order), ported the same way as
sdb-edge: task `http`, eight tickets (0-7), stages 1-10, RULES
`2026-10-15.4`. Design choices:

- **The single-shot board is frozen**: the existing 5-stage http suite is
  the base board; stages 6-10 (ranges, validators, compression, upload
  limits, hardening) run **only** when a campaign ticket accepts them, so
  no existing http history moves.
- **Depth is the HTTP semantics a production file server must have**:
  `Range`/`Content-Range` incl. suffix/open/416, `ETag` +
  `Last-Modified` with strict conditional semantics (stale validators
  never 304, rewritten content changes the ETag), gzip negotiation with
  real decoding (flate2) and a size check, chunked uploads and
  `Expect: 100-continue` without deadlock.
- **Limits run under a real cap**: the suite declares a 128MiB body and
  the pipeline restarts the candidate under a 128MB container cap for
  stages 9-10, so "reject at the head" is enforced by physics, not by a
  timeout. The contract advertises a 16MiB cap.
- **Hardening mirrors the neighbouring ladders**: traversal in raw,
  URL-encoded and double-encoded spellings (must never leak), 32KiB heads
  refused, malformed request lines 400, slow-loris reclaimed <=12s, all
  with normal traffic flowing.
- **Mock rigor**: the tests' mock file server implements ranges,
  validators with real date comparisons, gzip, chunked/continue uploads
  and the guards; ten injected defects each prove the intended stage
  catches them. The live proof (reference file server through the real
  pipeline) is the next step (Slice L5).
- Live proof (2026-10-15): the reference file server passed the full
  sdb-http ladder through the real pipeline (acceptance [1..10]; all 10
  stages PASS in ~56s), including the 128MB-capped restart for the
  upload-limit and hardening stages. Two findings: the reference initially
  missed the single-shot contract's `Content-Type: text/plain` (the
  verifier caught the fixture, not a harness bug), and the hardening
  stage's garbage-line check was widened to accept any 4xx or a clean
  close (RULES `2026-10-15.5`) so a correct 405/501 is not failed.
- First model cell (2026-10-15): ling-3.1-flash reached 6 on sdb-http
  (seven tickets first try, $0.1560, tickets/$ 44.9); the hardening ticket
  failed by one stage — the initial 405-for-garbage failure was a stale
  binary (RULES 2026-10-15.5 accepts any 4xx), and the model's repair then
  broke the 8KiB head cap (200 for a 32KiB header block). Launch hygiene
  note: rebuild the release binary after any grading change before
  launching cells (two cells have now lost verdicts to a stale binary).

## 18. Operator decisions on bench noise: proxy prompt clarity + readiness grace (2026-10-15.6)

Both changes are grading/spec semantics, hence a RULES bump (`2026-10-15.6`):

- **Egress-proxy prompt clarification** (proxy task prompt + sdb-edge
  genesis prompt): the candidate container sets
  `http_proxy`/`https_proxy`, so `curl` and similar tools may route
  through an egress proxy and fail on internal hostnames (`upstreamN`).
  The prompts now say to pass `--noproxy '*'` when probing and that the
  server must dial upstreams with raw sockets and ignore those env vars.
  Rationale: one model burned ~5 turns on this, which measures tool
  environment archaeology, not proxy-building skill.
- **Readiness grace** (proxy + http tasks): the pipeline's port gate is a
  TCP accept, so a candidate that binds early but serves late (slow
  startup self-test) could eat the verifier's single first request and be
  failed for an availability quirk unrelated to its implementation. After
  the port accepts, the pipeline now probes `GET /` for up to 20s and
  logs either "answered a readiness probe" or a warning before grading.
  Bounded: a genuinely dead or wedged server is still graded as-is (the
  verifier fails it), the harness just gives slow starters a fair window.
  A test pins the probe's three cases (serving / closed /
  accepts-but-silent-must-timeout).
- Remaining bench work (not in this change): the TLS ticket and the proxy
  throughput ticket (baseline gate) are their own waves; CI re-enable
  needs the GitHub-side Actions toggle (the workflow + 80% gate are in the
  repo); a coverage-margin wave is available on the biggest residuals.

## 19. sdb-sql accepted; reference isolation becomes repo-wide (2026-10-15)

The operator drafted `docs/PROPOSAL-sdb-sql.md` (one SQL-database
campaign, 21 tickets in 7 tiers, differential oracle against bundled
SQLite, lite variants, data-migration upgrade checks) and directed
"fix issues, document, build". This supersedes DECISIONS #11's four-app
portfolio and the single-shot boards once sdb-sql lands; the old specs,
grids and boards stay as archived history (retirement is phased, Q6).

Pre-build review resolutions are folded into the proposal (section
"Review resolutions (2026-10-15, pre-build)"): coverage gating only where
the harness computes it, pinned STATS semantics, a deterministic 5s-cancel
query, an explicit oracle discard table + per-stage comparable-check
floors + generator profile-coverage assertions, a pinned error-class
mapping, `reached` defined as highest stage passed, wall/disk guards, a
pinned `psql` client container, the rollup/portal shape, rollback-aware
prompts, and full rollback scope.

**Shipped with this entry (repo-wide fix)**: reference containers are now
detached from the run network for every verification window
(`detach_references`/`reattach_references`, wired around all
`run_verification` calls, including repair rounds) so no candidate can
proxy to `ref-*` and fake stages; the agent and the bench keep them. A
live docker test pins resolve → unreachable → resolve.

## 20. The sdb-sql rules freeze at `2026-10-19.41` (2026-10-26)

**Frozen:**

- `RULES_VERSION` `2026-10-19.41`, deployed and shown in `/api/status`
  (rounds 29-30).
- The run stamp `fingerprint()` (`src/report/rules.rs`) over the inputs
  in `src/report/graded-files.txt`.
- The graded paths in `scripts/graded-paths.txt`, which key the gate's
  scope and the flake record.

**Bump to `2026-10-19.42`** (round 36, 2026-10-26, the first bump after
the freeze, under the rules below):

- Each run records the sandbox image ID, and a career phase chains only
  onto a phase with the same image (`docs/rounds/36.md`, D1 a).
- `update_sandbox_image` passes the host's UID and GID to the image
  build.
- An empty stage-4 sentinel dataset retries the next seed (up to 16). A
  seed that built before keeps its exact dataset.
- Careers restart at phase 1. No career on `.41` was published.
- The 20-pass flake bar on this content runs after round 37.

**Evidence:**

- **The first flake bar**, on `5ebad6e`: 249 of 250 runs passed. The one
  failure was a stage-18 `infra_error` with no log. It didn't come back
  in 20 repro runs. Round 31 makes a failure keep its full log.
- **The second bar**, on `7f7e15b` (rounds 30 and 31): every one of the
  25 proofs passed 20/20, 500 runs in 16.6 h. Record
  `flake-50f1d60f0002.txt`. The rounds after it change no graded path.
- **CI:** GitHub run `40294023406` on `4d037c7` was green, after
  rounds 29-35 (`docs/rounds/29.md`, "Fifth run"):
  - 1044 passed and 0 failed.
  - Line coverage of the active code was 83.23 %, against the ratchet
    of 83.
  - The ratchet was 84 over all code. Round 35 excluded the 8 retired
    verifiers (`src/verifier/{redis,http,proxy,pdf,wal,queue,binproto,pubsub}.rs`)
    and measured again.
  - CI skips two tests that need the bench host: the `pdf-ref` image,
    and the upstream pool's readiness.

**Operator decisions** (2026-10-26, `docs/rounds/34.md`):

- **Bumps.** A rules bump needs its own round and the operator's
  go-ahead. Its `src/report/rules.rs` entry gives the score impact, and
  careers restart at phase 1. Published results name their version.
- **Graded-path changes without a bump.** These need the full gate and
  a new 20-pass flake bar before the next published cell.
- **CI first.** The freeze waited for a green GitHub run.

**Known gaps at the freeze** (each with the round that takes it):

- **The sandbox image isn't stamped.**
  - `Dockerfile.sandbox` installs Rust `stable` and unpinned apt
    packages, so a rebuild can change the candidate's environment under
    the same stamp (`docs/rounds/32.md`, "Out of scope").
  - Taken by the first round after the freeze: stamp the image ID at run
    time. **Done in round 36 (`.42`).**
- **Real cells assume the host user is UID 1000**, the image's
  `ubuntu`. CI gets around it with a build argument (round 33); the bench
  doesn't. Taken in the image-stamp round. **Done in round 36.**
- **The stage-18 `infra_error`:** 1 failure in 40 stage-18 runs (770
  proof runs in all), cause unknown. If it happens again, the run keeps
  its log (round 31).
- **Two tests share host port 15380.**
  - The mock Redis flow test (worker 90) starts its Redis reference on
    `worker_port(6380, 90)` (`src/pipeline.rs:4890`).
  - The SQL stage-6 proof (worker 99) gives its candidate
    `worker_port(5480, 99)`.
  - Both run in parallel in the gates and in CI, so either can fail with
    "port is already allocated". It happened once, in round 34's first
    fast gate. The flake bar runs the SQL proofs one at a time, so it
    can't hit this.
  - Taken in round 37 (moved from the image-stamp round), with a test
    that no two tests share a host port. **Done in round 37.**
- **The stage-4 dataset faults for about 1 seed in 5,000.**
  - `sentinel::build` (`src/verifier/sql/sentinel.rs:114`) returns an
    error when a seed's 40-step script leaves no writes or no ordered
    reads.
  - The persistence scenario reports that as a verifier fault
    (`src/verifier/sql/scenarios.rs:86`), and the run ends as
    `infra_error` (`src/verifier/sql/driver.rs:338`). It is never a
    candidate failure.
  - A scratch scan found 4 bad seeds in 20,000. It hit round 34's fast
    gate once, in the stage-10 proof.
  - Taken in the image-stamp round, with the rules bump that round needs:
    move to the next seed until the dataset isn't empty. That changes
    only the seeds that fault today. **Done in round 36 (`.42`).**
- **Small findings**, for a later round:
  - R30-2: the literal line break in `assert_logged`.
  - R31-2: the `.expect` on `BenchmarkPipeline::execute` prints no log.
  - The six clippy lints under Rust 1.99 (`docs/rounds/29.md`, run 1).
    The 1.92.0 pin holds them off.
  - `scripts/flake-bar.sh`'s header still names the record
    `<commit>.flake.txt`. It is `flake-<key>.txt`.
- **CI skips two tests** (round 35):
  - `live_test_reference_pdf_starts_dual_attached` needs a locally
    built image.
  - `test_pipeline_eval_only_proxy_full_ladder_live` fails because the
    nginx upstream pool is counted as ready before it serves.
  - Taken in round 37 (moved from the image-stamp round): a readiness
    probe for the pool, and a pdf test that skips itself when the image
    is missing. That round removes the skips and raises the ratchet.
    **Done in round 37:** CI runs both. The ratchet stays 83 (measured
    83.56 %, rounded down).
- **The retired campaigns' code** (verifiers, `tasks/`, `campaigns/`) is
  still compiled, only so that old rows stay readable (#19). Deleting it
  touches graded paths, so it's its own round after the freeze.

## 21. Dollars bind; wall time is a guard (2026-10-22)

The benchmark answers "what does a dollar buy" (round 23):

- **The cap.** `CELL_BUDGET_USD` is $0.99 per phase
  (`src/scoring.rs:141`), keeping the "Under-$1" identity.
- **The budget counts the API bill only**, at list price. The compute
  floor is reported, not budgeted.
- **Turn limits are runaway guards.** Money, not turns, ends a session.
- **Wall time is a guard.** `CELL_WALL_MIN` is 480, about 8 h per phase
  (`src/scoring.rs:174`).
- **Bumps.** Rules bumps were free while sdb-sql was in development,
  until #20.

## 22. Base rates, said on the page; all dev before the long tests (2026-10-30)

- **Q8 (a).** The snapshot keeps pricing every call at the model's base
  rate, with no rules bump. The results page says so and names each grid
  model's overrides (Haiku 5.5: 5x the prompt rate above 100k prompt
  tokens; gpt-6-luna: 2x above 272k; deepseek-v4.1-flash-1017: 2x in
  weekday peak hours). The text goes in round 51.
- **All development first.** The 20-pass bar, the load bar, the perf
  baseline and the calibration career run only after every planned
  round has merged (operator: "again i want to have all dev done before
  long tests"). A graded change after a bar would need a new one. The
  plan, all before the long tests: 43 (now), 50 (alongside 43), 47
  (after 43), 51, and 52 (round 40 folds into it). Findings R41-3,
  R41-4 and R42-4 close: AGENTS.md already asks for one item per commit
  and for Spec questions first.
- **The order after development** (operator, 2026-10-30, "smoke yes"):
  1. every planned round merged;
  2. the deploy;
  3. the perf baseline (round 49, "After the merge");
  4. one smoke phase-1 cell in 2 lanes with the test model, under
     $0.50 each, not publishable;
  5. fixes for anything 3 or 4 finds;
  6. a push, so CI runs (round 43 F4);
  7. the 20-pass bar and the load bar;
  8. the calibration career;
  9. the grid.
