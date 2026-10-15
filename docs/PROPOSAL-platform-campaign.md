# Proposal: sdb-platform — one campaign, one codebase, lite fallbacks

Drafted 2026-10-15. Status: **superseded** by `docs/PROPOSAL-sdb-sql.md`
(rejected: weak coupling between subsystems, heavily memorized products).
Originally: supersedes the
four-campaign portfolio (DECISIONS #11) once landed.

## TL;DR

Merge sdb-kv, sdb-queue, sdb-http and sdb-edge into **one ladder on one
codebase**: a storage platform that grows from a RESP key-value store into
a durable, queue-carrying, HTTP-fronted, hardened, replicated system. 34
stages in 8 tiers of rising difficulty.

A failed stage no longer ends the run: the workspace rolls back to the
last green commit and the model gets **one lite variant** of the same
capability. If the lite also fails, the stage is marked failed and the
ladder **continues**; only stages that need the missing capability are
skipped.

Headline: **difficulty-weighted points / campaign effective $**, plus a
per-capability profile (full / lite / failed / skipped).

## Operator decisions (2026-10-15)

1. **One combined campaign**, harder stages as it climbs.
2. **Failure drops to a lite variant only.** No stretch variants; a pass
   simply moves on.
3. **Retire the four campaigns** (sdb-kv, sdb-edge, sdb-http, sdb-queue).
4. **Retire the single-shot tasks** (redis, http, pdf, wal, proxy, pubsub,
   binproto, queue).

Carried over unchanged from the campaign design: green-field genesis, model
picks the language, stdlib only, cumulative acceptance, regression = a check
green earlier and red now, fresh agent session per stage, git is the
handoff, budget stop records `reached` and is never a model failure.

## Why

- **Information per $.** Today the ladder stops at the first failed ticket
  (`src/campaign.rs:444`). One syntax error at stage 0 (sdb-queue cell 2)
  or one gzip/range interaction (sdb-http cell 2) hides every later stage.
  With fallbacks + continue, every run measures the whole ladder it can
  afford.
- **Realism.** Real developers ship a reduced scope and keep going; they do
  not stop work on the product.
- **One difficulty curve.** Four separate ladders have four unrelated
  difficulty scales (sdb-queue is saturated by ling at 8/8; sdb-kv stages
  5-13 have never been reached). One ladder, one curve, one board.

## The ladder

Ports, all in one process from one `./start.sh`: RESP **6379**, queue
**6385**, SDB1 admin **6395**, HTTP **8080**, dashboard **8090**, edge
**8091** (separate process from the same repo; see Q4).

`Suites` names the existing verifier stage ids reused (redis `r`, queue
`q`, http `h`, proxy `p`). `Lite` lists the fallback planned for v1; `--`
means a fail on that stage is final for the stage (the ladder continues).

| # | Tier | Stage | Suites | Needs | Lite (v1) |
|---|------|-------|--------|-------|-----------|
| 0 | 1 Core | Genesis: RESP KV MVP | r1 r2 r4 | -- | -- (fail = reached none) |
| 1 | 1 Core | Expiry / TTL | r3 | 0 | -- |
| 2 | 1 Core | Pub/Sub | r5 | 0 | -- |
| 3 | 2 Durable | AOF + SIGKILL recovery | r6 | 0 | fsync each write, torn-tail tolerance not required |
| 4 | 2 Durable | Incident: disk full | r8 | 3 | -- |
| 5 | 3 Queue | Queue genesis: enqueue / lease / ack | q1 | 0 | -- |
| 6 | 3 Queue | Visibility leases + ownership | q2 | 5 | -- |
| 7 | 3 Queue | NACK + retry delays | q3 | 6 | immediate retry, no delay |
| 8 | 3 Queue | Dead letters | q4 | 7 | -- |
| 9 | 3 Queue | Queue survives kill -9 | q5 | 5, 3 | -- |
| 10 | 3 Queue | Many consumers, no duplicates | q6 | 6 | -- |
| 11 | 3 Queue | Delays + redrive | q8 | 8 | -- |
| 12 | 4 HTTP | HTTP front door: root / 404 / echo / headers | h1-h4 | 0 | -- |
| 13 | 4 HTTP | Files: upload + retrieve | h5 | 12 | -- |
| 14 | 4 HTTP | Byte ranges | h6 | 13 | -- |
| 15 | 4 HTTP | Conditional GET | h7 | 13 | -- |
| 16 | 4 HTTP | Compression negotiation | h8 | 13 | gzip on full 200s only; ranges stay identity |
| 17 | 4 HTTP | Chunked uploads + size limits | h9 | 13 | -- |
| 18 | 5 Ops | Metrics: INFO + queue counters | r7 q7 | 0, 5 | -- |
| 19 | 5 Ops | Tests + CI gate (80%, sensitivity) | r9 | 0 | 60% coverage; sensitivity check kept |
| 20 | 5 Ops | Dashboard (HTTP + SSE) | r10 | 18 | -- |
| 21 | 6 Hardening | RESP security (auth, caps, fuzz, slow-loris) | r11 | 0 | -- |
| 22 | 6 Hardening | Queue limits + hostile input | q9 | 5 | -- |
| 23 | 6 Hardening | HTTP hardening | h10 | 12 | -- |
| 24 | 7 Distributed | Pipelining perf gate | r12 | 0 | -- |
| 25 | 7 Distributed | 3-node HA | r13 | 3 | replication + manual `PROMOTE`, no election |
| 26 | 7 Distributed | Edge: routing + balancing | p1-p3 | -- | -- |
| 27 | 7 Distributed | Edge: failover + fail-fast | p4 p6 | 26 | -- |
| 28 | 7 Distributed | Edge: health + /status | p7 | 26 | -- |
| 29 | 7 Distributed | Edge: TTL cache | p8 | 26 | -- |
| 30 | 7 Distributed | Edge: limits + hostile input | p9 | 26 | -- |
| 31 | 8 Engine | Disk-resident store under memory cap | r14 | 3 | -- |
| 32 | 8 Engine | Compaction | r15 | 31 | -- |
| 33 | 8 Engine | Admin binary protocol (SDB1 v2) | r16 | 0 | -- |

v1 lites target the stages that end runs today or are expensive single
points of failure: durability (3), NACK delays (7), gzip interaction (16),
tests gate (19), HA (25). More lites are added only where grid data shows
a stage killing runs.

Budget: per-stage budgets carry over from the source campaigns; the sum is
about $5 per full run, plus lite attempts. The campaign-wide cap stays a
real stop.

## Variant mechanics

1. **Attempt full.** As today: initial turns + one repair round with
   feedback.
2. **On fail: roll back.** `git reset --hard` the campaign workspace to the
   last stage commit whose verdict was pass. The failed attempt is kept as
   a tagged commit (`sN-full-fail`) for audit, not as the base.
3. **Attempt lite.** Fresh session; the lite ticket text plus a short
   note that the previous attempt was rolled back, and the names (not the
   bodies) of the checks it failed. Its own budget and turn cap.
4. **On lite fail: roll back again, mark the stage failed, continue.**
5. **Skip by `needs`.** A stage whose `needs` include a failed stage is
   recorded `skipped` and costs nothing. A lite pass satisfies `needs`;
   the lite variant must therefore still provide the interface later
   stages rely on (e.g., HA-lite still runs three nodes with `ROLE`).

Cumulative suite: union of the suites of every stage that passed, at the
variant that passed (a lite pass contributes its lite checks, not the
full ones). Failed and skipped stages contribute nothing. Regression rule
unchanged: a stage that turns an earlier green check red fails.

## Scoring

- Stage points = tier weight x (1.0 full | 0.4 lite | 0 failed/skipped).
  Tier weight = tier number (1..8) as the v1 default (Q1).
- **Headline: points / campaign effective $** (same effective-cost formula
  as today: API + compute floor).
- Reported, not scored: capability profile (per stage: full / lite /
  failed / skipped), cost curve, turns, regressions caught, reached.
- Perf stays a gate. No throughput multiplier anywhere on this board.

## Retirement plan

Doctrine kept: archive and annotate, delete only with a reason (DNS
precedent, DECISIONS #2).

**Four campaigns.** `campaign` refuses `sdb-kv`, `sdb-edge`, `sdb-http`,
`sdb-queue` for new runs (an explicit flag still allows eval-only replay of
archived workspaces). Specs stay in `campaigns/` as history; their prompts
are the source text for platform stages. Their grid logs stay in
`TODO_CAMPAIGNS.md`; their boards are frozen and labeled retired in the
portal.

**Single-shot tasks.** `run`, `matrix`, `enqueue` and the UI Setup tab
stop offering redis, http, pdf, wal, proxy, pubsub, binproto, queue.
`eval`, `replay`, `leaderboard` and the portal keep reading history.
Batch-anchored throughput denominators (redis/http) retire with them.

**Verifier code.**
- Kept, as platform suites: `verifier/redis.rs`, `verifier/http.rs`,
  `verifier/proxy.rs`, `verifier/queue.rs`, `binproto::admin_stage`.
- Dormant after retirement: standalone `verifier/pdf.rs`, `verifier/wal.rs`,
  `verifier/pubsub.rs`, and the standalone binproto task path. Deleting
  them is a separate operator call (Q5); the 80% coverage gate must hold
  either way.

## Harness changes

| Need | Today | Change |
|------|-------|--------|
| Stage spec | `id, type, title, prompt, acceptance, budget, max_turns` | + `tier`, `needs`, `lite {prompt, acceptance, budget_usd, max_turns}` |
| Orchestrator | stop at first fail (`campaign.rs:444`) | rollback, lite attempt, continue, `needs` skipping |
| Acceptance | one task per campaign ("one of the lists stays empty") | several task suites per stage against one candidate on several ports |
| Stage executor | one verifier family per candidate | run redis + queue + http + proxy suites in one verification pass; port map per suite |
| Git | commit per stage | + reset to last green, tagged failed attempts |
| Rollup | reached, regressions, cost | + points, profile, skipped list |
| Retirement | DNS-style removal only | refuse-new-runs switch for tasks and campaigns; read paths untouched |
| RULES | per campaign version | new stamp; platform is its own board, never pooled |

The multi-suite stage executor is the largest piece. Everything else in the
orchestrator is pure logic and unit-testable without Docker or spend, like
the current `campaign.rs`.

## Open questions

- **Q1. Tier weights.** Tier number (1..8) as default, or calibrated from
  observed pass rates after the first grid?
- **Q2. Lite feedback.** Show the failed check names from the full attempt
  (proposed), full verifier output, or nothing?
- **Q3. HTTP files storage.** Filesystem (proposed for v1; the http suites
  work unchanged) or stored in the KV engine (more integration, new
  suites)?
- **Q4. Edge upstreams.** Harness fixture upstreams (proposed for v1;
  deterministic per DECISIONS #10) or the model's own HA nodes (more
  realistic, couples edge to stage 25)?
- **Q5. Dormant verifiers.** Delete pdf/wal/pubsub standalone code after
  archiving, or keep dormant?

## Rollout

1. Orchestrator: `tier`/`needs`/`lite`, rollback, continue, scoring, rollup.
   Unit tests only.
2. Multi-suite stage executor: one candidate, several ports, several
   verifier families. Live proof with a reference implementation.
3. Retirement switch: refuse new runs for old tasks and campaigns; history
   paths unchanged; portal labels.
4. Spec tiers 1-3 (stages 0-11) with lites for 3 and 7. Reference passes
   every stage; each lite suite passes 20/20.
5. Tiers 4-8 one at a time, lites for 16, 19, 25.
6. Calibration: one strong model end-to-end before any grid; drop or
   re-scope stages no model can pass.
7. Grid on the new stamp.
