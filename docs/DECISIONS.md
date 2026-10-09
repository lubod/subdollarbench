# Deferred decisions

One entry each; 5 lines max. Status + next trigger, not essays.

## 1. Throughput curve deliberately unchanged (2026-10-03.5)
`src/report/rules.rs` regime notes: verifier pins shipped, curve untouched —
zero anchored ratios in the fresh wave (5/5 drifted), so any retune is unverifiable.
Revisit when a wave yields anchored redis/http ratios under the bench lock.
Ref: `6891313`, `src/report/rules.rs:20-35`.

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
