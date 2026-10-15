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
