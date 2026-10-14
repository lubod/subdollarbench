# Proposal: protocol-aware ranking, throughput curve, Pareto reporting (2026-10-14)

Status: draft for operator review. No graded files change until approved
(see TODO_EFFORT §R for the shipped fix bundle). Evidence below is from
the 2026-10-14 canary re-run and offline simulation.

## 0. Context

RULES 2026-10-14 lengthened the redis measurement window (n=5000 -> 100000)
and fixed two scoring defects (n=1 drift quarantine; throttle non-results).
Live confirmation: the first three batch brackets under the new regime
measured 255,383 / 264,517 / 256,980 req/sec — within 4% of each other,
versus 87,746 vs 142,228 (62% apart) under n=5000. The denominator is now
a measurement, not a coin flip.

## 1. Ranking protocol (the biggest open validity item)

Situation: stop-on-pass makes every 100% cell n=1 (Wilson lo 20.7); a
fixed-n matrix 9/9 scores lo 70.1. Both can exist under one stamp.
`protocol` is recorded per parent but is not part of any comparability
gate.

Offline simulation (3000 cells/point, same grid):

| true p | stop-on-pass: E[n] / mean stat | fixed-n=4: stat | SPRT(0.3,0.8): E[n] / posterior mean |
|--------|-------------------------------|-----------------|--------------------------------------|
| 0.20   | 2.96 / 6.9                    | 4.9             | 3.16 / 29.5                          |
| 0.50   | 1.87 / 13.7                   | 17.7            | 5.19 / 48.9                          |
| 0.80   | 1.24 / 18.3                   | 35.4            | 4.35 / 71.6                          |
| 0.95   | 1.05 / 20.0                   | 46.5            | 3.31 / 78.7                          |

Findings:
1. Within one protocol, Wilson-lo ranking is monotone in p — no fix needed
   inside a protocol (the earlier "fast solver ranks low" read holds only
   across protocols).
2. Across protocols the comparison is invalid by construction. This is a
   board-gating bug, not an estimator bug.
3. Stop-on-pass buys almost no information about a strong model (n=1) at
   1.8 trials/cell; SPRT costs ~4.2 trials/cell and produces a statistic
   with real spread (29.5 vs 78.7 across the grid).

Options:
- A. Gate boards by protocol now (cheap, required regardless): rank only
  within (task, rules_version, harness, protocol); refuse mixed-protocol
  pooled rollups; matrix gains the same uniform-protocol rule it already
  has for effort. Protocol badge on rows.
- B. Confirmatory top-up: after a stop-on-pass solve, run `min(n,3)` extra
  trials so passing cells land at ~4/4 (lo 51). Costs 3 trials per solved
  cell; keeps the simple estimator.
- C. Adopt SPRT(0.3,0.8) for the method wave: LLR boundaries ±2.197,
  nmax 9, rank on the posterior mean (or Wilson-lo of the final n,c).
  Failing cells stop early (spend moves to where information is), passing
  cells cost ~4-5 trials instead of 1.

Recommendation: A immediately (validity), C as the method upgrade, B only
if SPRT is rejected. Note SPRT changes trial counts per cell, so it needs
its own RULES bump and re-run (same doctrine as every denominator change).

## 2. Throughput curve (deferred under DECISIONS #1, evidence now exists)

Current multiplier m(r) = r(1+2r): m(0.5)=1.0, m(1)=3.0, m(1.5)=6.0,
m(2)=10.0. Indifference reading: a 2x-faster model may cost 10x per solve
and still tie. With the denominator now stable (CV ~6% per window,
median of 5), the question is no longer "can we measure r?" but "what
tradeoff do we want to price?".

Options:
- Keep r(1+2r): superlinear reward; defensible only if speed is the
  primary systems-quality axis and ratios >1.5 are routinely real.
- Cap at 1.5: m(r)=min(r,1.5)*(1+2*min(r,1.5)) — m(2)=6, m(3)=6; treats
  extreme ratios as same-class speed.
- Linear m(r)=r: minimal assumptions; 2x speed = 2x score.
- Report-only: drop the multiplier from the headline, rank on
  P@1 / cost and publish the speed ratio as its own column (Pareto view).

Recommendation: linear or cap until a wave demonstrates that r>1.5 is a
reliable quality signal (e.g., speed ratio explains residual pass rate
after controlling for cost). Also add `bench_n` to `ref_history.jsonl`
rows and make `deviation()` compare only same-n regimes — the cross-batch
warning currently fires against the mixed n=5000/n=100000 history
(log-only today, but it should be regime-correct before it ever gates).

## 3. Pareto reporting (the practitioner artifact)

Replace the "which single score wins" framing with a frontier:
Pass@1 (with Wilson CI) vs $ per solve (API + compute floor), one point
per (model, task). The composite can stay in the table; the frontier is
the headline chart. This also makes the compute-floor constant visible:
publish the same frontier at 0.5x / 1x / 2x floor and show rank churn
(none of this needs new runs — it is arithmetic over archived rows).

## 4. Open questions for the operator

1. Protocol: adopt A only, or A+C (SPRT method wave)?
2. Curve: keep / cap 1.5 / linear / report-only?
3. Pareto frontier: regenerate portal section on the next drain? (no
   scoring change, display-only, no rules bump)
4. Canary verdicts: confirm the two P1 fixes on live rows before any of
   the above lands.
