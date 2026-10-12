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
- [x] R3a. (rebuilt; binary verified current before restart) Release build contains 9d9c2c8 (merge rule, template fix,
  health gate, stage labels, DQ markers) — rebuild to be sure.
- [x] R3b. (30s notice, both services, integrity + migrate clean, idle) Restart engine+gui, integrity + migrate-check clean.
- [x] R3c. (backfill: rewrote 10, re-stamped 29; pdf column live: 77.8/87.5/87.5/100.0/87.5/85.7 with DQ counts; proxy high recomputed 5.0/n=8) Backfill recompute (repair/backfill path): parents merge to
  n=valid + DQ markers; all-cheat rule holds.
- [x] R3d. (API 39 rows; annotate-cutoff re-run still TODO for new rows — q51+ wave rows unscanned) Verify: pdf column populated on matrix/API; proxy cells still
  flagged (reruns pending); `annotate-cutoff` re-run for new rows.

## R4. Proxy rerun wave (GATE: R3 green)
- [x] R4a. (q51–q56: 3 omp + 3 opencode × proxy × 9, fresh .4 rows; 4 running, 2 pending. Note: restart drops Done history from memory queue by design — DB/files untouched.) Enqueue 6 cells (3 omp + 3 opencode × low/high/max × 9) —
  fresh rows under fixed prompt (.4), old infra-capped cells stay
  archived + annotated.
- [ ] R4b. (watching: health-gate `[SETUP] ...recreated` lines? worker-port binding in containers? scores vs 0–40% baseline) On drain: upstream gate fired? (console `[SETUP] ...recreated`
  lines); candidates binding worker ports? (no 8091 in containers);
  scores vs 0–40% baseline.

## R5. Portal + analysis unblock
- [ ] R5a. (regen AFTER proxy wave lands — boards still miss legit proxy) Regen + push portal on landed data (boards now include pdf).
- [ ] R5b. §J deep analysis (variant effect n=9, recovery incidence,
  effective-vs-requested audit, taxonomy, cost, cross-harness, probes).

## R6. Small bugs noticed along the way
- [x] Extend-completion notes misreport — FIXED (1b6b818), root-caused
  deeper than cosmetic: stranded scored trials (trials table complete,
  parent missing them) + unconditional failure notes + len+1 numbering.
  Now: trials-table orphan absorb (pre-batch for numbering, post-batch
  race belt), max-N+1 numbering, honest notes (billing/cancel/N-errored/
  setup-refused), outcome-true history notes. Engine + both foreground
  extend paths. No scoring-math change: no rules bump.
- [ ] `annotate-cutoff` re-run for q51+ rows (new trials unscanned).

## Standing decisions (do not re-litigate)
- No retroactive rescoring; annotation + top-up + rerun only.
- Cheat cap 2 attempts/cell/round; re-cheats accepted.
- One RULES bump per bundle (.4 covers merge+proxy batch).
- No deploys/restarts/prune/queue-clear during live trials; 30s notice.
