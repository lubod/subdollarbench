# Round 2 — execute/check/test (2026-10-11)

Engine stopped mid-top-ups on user order; queue cleaned to terminal
history; 5-trial top-up round 2 queued (063015:+2 started, q49 +1, q50
+2 — all under .3). Checklist below runs in order; deploy gate last.

## R1. Missing regression tests (code-only, no engine needed)
- [x] R1a. `stage_label` unit test: strips `Stage N:` prefix, passes
  through names without/with wrong-number prefix.
- [x] R1b. (extracted upstreams_reachable helper; recreate path stays docker-gated) Upstream health-gate helper test: extract
  `upstreams_reachable(ports) -> bool`, test against a bound
  TcpListener (healthy) + closed port (refused). Recreate path stays
  docker-gated (no unit test; covered by proxy rerun wave).
- [x] R1c. (350 passed, fmt, clippy -D warnings clean) Full suite green: `cargo test --lib`, `fmt --check`,
  `clippy --all-targets -- -D warnings`.

## R2. Top-up round 2 watch (no action, just verify)
- [ ] R2a. All 5 land (063015 trial11+12, 085141 trial10, 123136 trial10+11).
- [ ] R2b. Any re-cheats → accept per cap (no round 3 without user word).
- [ ] R2c. Record final valid-n per pdf cell (low/high/max, omp+opencode).

## R3. Deploy .4 + backfill (GATE: engine idle + 30s notice)
- [ ] R3a. Release build contains 9d9c2c8 (merge rule, template fix,
  health gate, stage labels, DQ markers) — rebuild to be sure.
- [ ] R3b. Restart engine+gui, integrity + migrate-check clean.
- [ ] R3c. Backfill recompute (repair/backfill path): parents merge to
  n=valid + DQ markers; all-cheat rule holds.
- [ ] R3d. Verify: pdf column populated on matrix/API; proxy cells still
  flagged (reruns pending); `annotate-cutoff` re-run for new rows.

## R4. Proxy rerun wave (GATE: R3 green)
- [ ] R4a. Enqueue 6 cells (3 omp + 3 opencode × low/high/max × 9) —
  fresh rows under fixed prompt (.4), old infra-capped cells stay
  archived + annotated.
- [ ] R4b. On drain: upstream gate fired? (console `[SETUP] ...recreated`
  lines); candidates binding worker ports? (no 8091 in containers);
  scores vs 0–40% baseline.

## R5. Portal + analysis unblock
- [ ] R5a. Regen + push portal on landed data (boards now include pdf).
- [ ] R5b. §J deep analysis (variant effect n=9, recovery incidence,
  effective-vs-requested audit, taxonomy, cost, cross-harness, probes).

## Standing decisions (do not re-litigate)
- No retroactive rescoring; annotation + top-up + rerun only.
- Cheat cap 2 attempts/cell/round; re-cheats accepted.
- One RULES bump per bundle (.4 covers merge+proxy batch).
- No deploys/restarts/prune/queue-clear during live trials; 30s notice.
