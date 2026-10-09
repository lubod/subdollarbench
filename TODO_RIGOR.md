# Rigor roadmap — 5 slices (2026-10-09)

- [x] 1. Uncertainty display + free/paid split + canary (days, no grading change)
  - [x] 1a. Throughput/token IQR across trials on run + matrix views (Wilson stays the rank key)
  - [x] 1b. Split boards: free vs paid (replace display-only badges; no invented costs)
     * GUI + static portal parity: tier pills + IQR spread, helpers hoisted to
       shared.js (single source), portal JS parse-guarded by a node --check test
     * Cost honesty behind the split: `resolve_api_cost` (a zero live key-usage
       delta never zeroes a paid model; formula wins + source recorded), and
       `cost-annotate` backfilled `cost_formula_usd` onto the 144 archived
       muse-spark rows that recorded $0.00 from counter lag — annotation only,
       no score rewrites. Board verified: 17/17 muse rows now paid.
  - [x] 1c. Canary GUIDs in all 7 prompt templates + advisory workspace echo scan (RULES 2026-10-09.2)
- [x] 2. `replay <run_id>` (day; e2e validated 2026-10-09)
  - [x] 2a. Build archived workspace + run verifier, no API key (reuse `eval` path)
  - [x] 2b. CLI wiring + docs
- [ ] 3. Robustness probes → differential oracle (weeks, flake discipline)
  - [x] 3a. Malformed-frame + slow-close probes, unscored first (redis/http)
  - [x] 3b. Differential oracle vs reference on seeded random traffic (redis-only by design; nginx implements none of the task routes)
  - [ ] 3c. Promote to scoring only with zero-flake evidence; version bump per change
- [ ] 4. Breadth wave (needs spend approval)
  - [x] 4a. Verified 2026-10-09 against live OpenRouter catalog.
  IN (avg <= $1/M): gemini-2.5-flash-lite, gpt-4o-mini, llama-3.1-8b-it,
  llama-3.1-70b-it, qwen-2.5-coder-32b-it, qwen-2.5-72b-it, deepseek-chat,
  codestral-2508, ministral-3b-2512, qwen-2.5-7b-it.
  OUT (blueprint IDs stale or over budget): gemini-1.5/2.0-flash (gone),
  claude-3/3.5-haiku (gone; haiku-4.5 avg $3), gemini-2.5-flash (avg $1.40),
  devstral (avg $1.20), llama-3.3-70b/qwen-72b-instruct naming corrected. Verify which budget IDs still exist on OpenRouter
  - [ ] 4b. Approve spend; run uniform matrix (single effort + harness, n=9)
- [ ] 5. Paper (after 4)
  - [ ] 5a. Failure taxonomy from archived runs (deadlocks, framing, packaging)
  - [ ] 5b. Language-selection analysis (Go/Python/Rust under cost constraints)
  - [ ] 5c. Draft 6–8 pages; publish only on multi-model data

Rules: grading changes bump RULES_VERSION + fingerprint; no per-wave
protocol mutations (regime fragmentation); engine idle for deploys.

Deployed: YES (2026-10-09, engine idle window). Release restart of engine+gui
services clean; integrity 0 orphans/0 junk; repo portal regenerated
(17 paid / 57 free after cost-annotate).

Slice 2 e2e validated on a real archived run (muse-spark redis trial9):
compliance re-check PASS, differential oracle PASS against ref-redis,
verdict MATCH (100% = archived 100%). NOTE: no archived run carries
robustness data yet — the wave finished before 3a shipped, so this replay
is the first recorded probe evidence (Robust1 PASS, Robust2 FAIL: slow
frame got "$-1" where a bulk was expected). 3c promotion still wants
several flake-free rows.
