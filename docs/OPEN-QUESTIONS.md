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

