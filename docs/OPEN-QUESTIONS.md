# Open questions needing a product call

Collected 2026-10-14 (end of the campaign build day). Each item has the
data, the options, and a recommendation.

**All six answered by the operator on 2026-10-14 — resolutions inline.**

## Q1. Ticket 10 (three-node HA finale) — failover semantics & partition scope

**RESOLVED (option a): automatic failover, deterministic rule, T=10s;
partition check deferred to a follow-up.** Survivors promote the replica
with the highest replication offset (tie: lowest port) within 10 seconds
of the master's loss. The network-partition stretch check (isolate a
minority; majority serves; minority cannot elect itself) becomes a later
ticket/probe rather than v1 scope. Build follows the proposal's Stage 10
contract: `./start.sh --port <p> --data <dir> [--replicaof host:port]`,
nodes 7001-7003, `ROLE`, sync-ack, rejoin, full-suite re-run against the
cluster's current master.

## Q2. s3 (durability) pool calibration

**RESOLVED (option a): s3 pool raised 30 -> 36 turns.**
Initial cap ~24 + 12-turn repair. Data: ling failed s3 four consecutive
times in the same "repair ran out mid-refactor" class (runs 4, 5, 7, 8;
~36+ turns needed). Cost per attempt +~20%; unlocks live data for
tickets 4-9. Cells measured before this change used the 30-turn pool
(loose comparability, documented in the grid log). RULES
`2026-10-14.11`.

## Q3. Perf baseline visibility

**RESOLVED (option a): keep blind for v1.** The perf ticket's 1.3x
gate stays a prompt-level instruction without exposing the numeric
baseline; revisit (prompt templating) only if the grid shows clean
near-misses.

## Q4. Scoring protocol validity

**RESOLVED (option a): gate boards by protocol now.**
- `matrix-report::validate_comparison_wave` refuses to pool runs across
  protocols; legacy rows group as `unknown` (never mix with a stamp).
- `merge_subruns` stamps a parent `mixed` when its subruns disagree
  (previously it silently took the first protocol).
- Portal rollups group and badge by (rules regime, protocol): pooled
  statistics never span comparability classes.
SPRT (option C) and top-up (option B) are deferred until a method wave
needs them. RULES `2026-10-14.11`.

## Q5. Throughput curve

**RESOLVED: linear pricing, score = base efficiency x speed_ratio**
(was r(1+2r), where a 2x-faster model could cost 10x per solve and tie).
Revisit only if a wave shows r > 1.5 is a reliable quality signal.
RULES `2026-10-14.11`.

## Q6. Bench's own CI coverage gate

**RESOLVED (against the recommendation): raise to 80% now.**
`.github/workflows/ci.yml` now enforces
`cargo llvm-cov --fail-under-lines 80`, dogfooding the coverage standard
the campaign demands of models. GitHub Actions remains disabled, so the
gate is inert until CI is re-enabled/scoped; the bench measured ~70%
lines when raised, so re-enabling CI requires coverage work on the
docker-gated paths (tracked in TODO_CAMPAIGNS hygiene) or a measured
exemption.

## Q7 (display-only). Pareto frontier regeneration

No scoring change; regenerate the portal's frontier section on the next
drain. (Accepted by default; no answer needed.)

## Q8. Tiered and time-of-day prices (decided 2026-10-30: (a), `DECISIONS.md` #22)

The snapshot (`src/cost/model_prices.json`) prices every call at the
model's base rate. The OpenRouter catalog now lists overrides: Haiku 5.5
charges 5x the prompt rate above 100k prompt tokens, gpt-6-luna 2x
above 272k, and deepseek-v4.1-flash-1017 2x
in weekday peak hours. A career's prompts reach several hundred
thousand tokens, so for some models the scored cost (and the $0.99 cap)
sits below the bill. Options: (a) keep base rates and say so on the
results page; (b) price the overrides in the snapshot (a rules bump);
(c) cap context so no call crosses a threshold. Found in round 46.


## Q9. The crash-safety gate (open, 2026-11-04; decide after the v2 grid)

**The finding.** Ticket 5 (crash safety) gates most of phase 4 through
the chain 4 → 5 → 6. Of the phase-4 tickets, 17 needs 5, and 16, 18, 19
and 20 need 6 (`campaigns/sdb-sql/campaign.json`, `needs`). A career
that can't pass 5, or its lite variant, scores 0 in phase 4: up to 32 of
76 points (42%), whatever else it can do.

**The evidence.** gpt-6-luna, grid career c2, rules `.45`:
- It failed ticket 5 five times: full and lite in phase 1, then lite as
  a catch-up in phases 3 and 4. Each catch-up target gets one attempt
  per phase, and there are 3 slots (`CATCH_UP_SLOTS`,
  `src/scoring.rs:33`; `catch_up_targets` in `src/campaign.rs`).
- Its store rewrites one `database.json` per commit, and it never came
  back after a torn tail.
- Its phase 4 ended after 17 min with $0.94 of its $0.97 unspent, and 16
  to 20 were skipped. The career ended at 27.4 points.

**This is by design.** Ticket 6's scenario is a crash inside an open
transaction (`PROPOSAL-sdb-sql.md:165`), and "transactions meet the WAL"
is intended coupling (`:53`). The ladder measured a real gap. The open
issues are:

1. **The gate's weight:** a single capability decides 42% of the points.
2. **Unspent budget:** when the blocker's one attempt per phase fails and
   nothing else is runnable, the phase ends with its budget unused.
3. **Variance:** if one career of a model passes 5 and another doesn't,
   one ticket swings about 30 points. Publishing needs the spread
   between models to exceed the spread within one model (`TODO_CAMPAIGNS.md`,
   "The bar").

**Options.**
- **(a)** Change nothing, and report it: "points blocked by gate X" per
  career, and the survival curve P(ticket ≥ i).
- **(b)** When nothing else is runnable, the blocker gets further
  attempts in the same phase until the budget runs out: full after lite,
  with fresh feedback. This keeps the `needs` graph and stops wasting
  budget. It needs a rules bump, and careers restart.
- **(c)** Loosen the chain. Ticket 6 needs only 4, and the "crash inside
  a transaction" check moves to 17 or a late check. This changes what the
  ladder means, and needs a rules bump.
- **(d)** Partial credit for skipped tickets. This contradicts the
  cumulative design, and is not recommended.

**The next step, after the v2 grid (reviewer).** For each of the 15
careers, record:
- which gate, if any, blocked it, and the points it blocked;
- the budget left when each phase ended;
- whether careers of the same model differ on ticket 5 or 6.

Then run the variance check. The recommendation:
- **(a)** if the spread within each model stays below the spread between
  models;
- **(b)**, as a rules `.46` round, if gate events drive the spread;
- **(c)** only on the operator's call that crash safety should not gate
  the top tier.

No change during the running grid: any of (b)-(d) restarts all careers
(about 30 h and $27).

**Measured** (reviewer, 2026-11-06 14:10 CET / 13:10 UTC; the v2 grid, 15
careers, rules `.45`, key `a249e9c32c47`).

| model | careers (points) | median | spread |
|---|---|---|---|
| deepseek-v4.1-flash | 63.0 · 65.0 · 70.0 | 65.0 | 7.0 |
| mimo-v2.6-flash | 24.4 · 62.8 · 67.0 | 62.8 | 42.6 |
| claude-haiku-5.5 | 55.6 · 57.6 · 64.6 | 57.6 | 9.0 |
| gpt-6-luna | 27.4 · 44.0 · 47.0 | 44.0 | 19.6 |
| qwen3.8-flash | 25.0 · 37.0 · 37.0 | 37.0 | 12.0 |

- **The variance check:** the median within-model spread is 12.0; the
  model medians span 28.0, a ratio of 2.3. The adjacent median gaps are
  2.2, 5.2, 13.6 and 7.0.
  - The top three are not separable from each other.
  - The groups are separable: haiku's worst (55.6) beats luna's best
    (47.0).
  - Rank stability over all 243 combinations: deepseek #1 in 70%.
- **The gates drive the outliers:**
  - Two careers failed ticket 5 and lost all of phase 4: mimo c13
    (24.4, also 0 in phase 1 on its ticket-0 budget) and luna c2 (27.4).
    They ended phase 4 with $0.90 and $0.96 unspent.
  - Qwen c14 failed ticket 6, and 16, 18, 19 and 20 were skipped.
  - Without the gated careers, the spreads are deepseek 7.0, mimo 4.2,
    haiku 9.0, luna 3.0 and qwen 0.0.
- **Reading against the rule above:** the aggregate test passes (12.0
  within < 28.0 between). But gate events cause the two largest spreads.
  Both conditions hold, so this is the operator's call: (a) publish with
  the gates shown per career, or (b) a rules `.46` round and a new grid.

