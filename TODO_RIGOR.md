# Rigor roadmap — 5 slices (2026-10-09)

- [ ] 1. Uncertainty display + free/paid split + canary (days, no grading change)
  - [x] 1a. Throughput/token IQR across trials on run + matrix views (Wilson stays the rank key)
  - [x] 1b. Split boards: free vs paid (replace display-only badges; no invented costs)
  - [ ] 1c. Canary string in all prompt templates + workspace seeds
- [ ] 2. `replay <run_id>` (day)
  - [x] 2a. Build archived workspace + run verifier, no API key (reuse `eval` path)
  - [x] 2b. CLI wiring + docs
- [ ] 3. Robustness probes → differential oracle (weeks, flake discipline)
  - [x] 3a. Malformed-frame + slow-close probes, unscored first (redis/http)
  - [x] 3b. Differential oracle vs reference on seeded random traffic (redis/http)
  - [ ] 3c. Promote to scoring only with zero-flake evidence; version bump per change
- [ ] 4. Breadth wave (needs spend approval)
  - [ ] 4a. Verify which budget IDs still exist on OpenRouter
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
