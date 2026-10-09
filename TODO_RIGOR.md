# Rigor roadmap — 5 slices (2026-10-09)

- [ ] 1. Uncertainty display + free/paid split + canary (days, no grading change)
  - [x] 1a. Throughput/token IQR across trials on run + matrix views (Wilson stays the rank key)
  - [x] 1b. Split boards: free vs paid (replace display-only badges; no invented costs)
     * GUI + static portal parity: tier pills + IQR spread, helpers hoisted to
       shared.js (single source), portal JS parse-guarded by a node --check test
  - [ ] 1c. Canary string in all prompt templates + workspace seeds (pending idle — prompt edits contaminate running trials)
- [ ] 2. `replay <run_id>` (day)
  - [x] 2a. Build archived workspace + run verifier, no API key (reuse `eval` path)
  - [x] 2b. CLI wiring + docs
- [ ] 3. Robustness probes → differential oracle (weeks, flake discipline)
  - [x] 3a. Malformed-frame + slow-close probes, unscored first (redis/http)
  - [x] 3b. Differential oracle vs reference on seeded random traffic (redis/http)
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

Deployed: NO — engine running q50 (restart would kill trials). Ship 1a+1b with 1c at idle.

Slice 2 e2e validation pending idle (engine running q50; replay refuses while live by design).
Slice 1c canary pending idle (prompt edits would contaminate running trials).
