# Effort parity + cutoff recovery — implementation TODO (2026-10-09)

Bundle: give both harnesses machine-level effort control, recover (not
reclassify) turn-1 reasoning cutoffs, fix turn telemetry, annotate history.
Plus consolidated remaining roadmap (§H). Code-only except marked gates.

Decisions already taken (not re-litigated here):
- No prose clamp in OpenCode: effort stays `max` where requested. OMP's
  flag clamp is replaced by attempt-then-fallback (§C), not extended.
- No retroactive reclassification of archived 0% rows (verdict-honesty
  rule, DECISIONS #7). History gets annotated (§F), never rescored.
- Effort is harness-relative by construction (`max` = most the
  harness+provider path supports); ranks never mix harnesses, so no
  cross-harness comparability is claimed.

## A. Turn telemetry fix (no RULES bump — display-only field)

- [x] A1. Copy `turns` into `st.steps_taken` on all 4 return paths in
  `src/sandbox/opencode.rs` (billing-Err ~632, success-Ok ~649,
  limit-Ok ~656, timeout/cancel-Err ~659/678/687). Scope confirmed:
  171/171 opencode manifests read `turns: 0` vs 0/441 omp.
- [x] A2. Unit test: synthetic `step_start` event stream →
  assert `steps_taken` propagated on Ok and on Err partial_stats.
- [x] A3. `cargo test --lib`, `fmt`, `clippy -D warnings`.

## B. OpenCode machine effort: `--variant` wiring

- [x] B1. Zero-spend introspection (static, host CLI 2.0.26 binary):
  `-m` accepts `provider/model#variant`; the `openrouter` provider package
  maps effort variants → native `settings: {reasoning: {effort}}`
  (OpenRouter's own reasoning parameter — NOT ignored), and
  budget-tokens variants → `reasoning: {max_tokens}`. Default variants
  auto-generate when the catalog model lacks explicit ones. So `#max`
  should resolve without custom config; live resolution check is B2b.
  NOTE: host CLI is 2.0.26, sandbox pins 2.0.24 — re-verify in image at G2.
- [x] B2. (B2a proven live $0; B2b wired unconditionally — default catalog lacks our models; B2c prose untouched by design) Wire `#variant`: append `#<effort>` to `-m` in `make_cmd`
  (effort vocabulary low/high/max + minimal/medium/xhigh/off mapped to
  same-named variants; `""|default|auto` → `#high` matching OMP).
  a. Live resolution check first ($0 on ling-3.1-flash): trivial run with
     `#max`; resolution error = unavailable (zero spend either way).
  b. `OPENCODE_CONFIG_CONTENT` custom variants only if B2a fails or a
     level is missing (binary shows auto-generation should cover us).
  c. Prose effort line stays untouched (belt-and-braces).
- [x] B3. Add `.arg("--variant").arg(effort)` next to `-m` in `make_cmd`;
  unknown-variant resolution error → fall back to no-variant (prose-only)
  + log line. Never fail the trial on variant config.
- [ ] B4. Forwarding check: binary evidence says the openrouter package
  forwards effort natively, but verify EFFECT not just resolution —
  reasoning-token volumes must move across variants on the same prompt
  (G2 canary). If inert, raw `body` passthrough
  `{"reasoning": {"effort": ...}}` is the backup. Investigate-option:
  `budget_tokens` variants (`reasoning.max_tokens`) as a hard cap on
  runaway thought — semantics of cap-hit unclear, do not ship blind.
- [x] B5. Unit tests: variant-config JSON shape per effort level;
  fallback path on resolution error. Full suite + fmt + clippy.

## C. OMP: unclamp (evidence killed the fallback plan)

Measured 2026-10-09 ($0 probes on ling): `--thinking=max` is ACCEPTED
(exit 0) and the CLI records `thinkingLevel: "high"` in the session —
it downgrades unsupported levels itself, no HTTP 400 exists on this
path. Retry machinery would guard a hypothetical, so it was cut.

- [x] C1. `--thinking=<requested>` verbatim (pre-clamp removed). No
  retry loop: nothing to retry (see measurement). If a 400 ever
  materializes in logs, build fallback then (YAGNI until evidence).
- [x] C2. Kept `""|default|auto → high`; `test_resolve_thinking_level`
  updated (max passes through for every provider).
- [x] C3. `[THINKING]` telemetry arm on `format_session_line` (the live
  tailer path): the session's `thinking_level_change` event renders the
  EFFECTIVE level next to the `[OMP-PROMPT]` requested level, so CLI
  downgrades are visible, never silent. Unit tests for both.

## D. Resume-on-cutoff recovery (opencode)

- [x] D1. Trigger in the attempt loop: exit-0 AND 0 `tool_use` events
  AND empty workspace (no start.sh/Dockerfile/files). All three signals
  already observed by the runner — no session-DB dependency.
- [x] D2. (cap 2; same turn/budget limits; turns accumulate via §A fix) Recovery = resume same `--session` with injected user message
  ("previous response cut off before creating files — start executing
  tools now"), under the same turn/budget limits. Cap 2 recoveries per
  trial; third strike accepts the 0% as genuine model failure.
- [x] D3. Log `[CUTOFF-RECOVER]` with attempt number; trial continues as
  a normal trial (no new status, no rescoring path).
- [x] D4. Unit tests with synthetic event streams: trigger fires only on
  all-three-signals; cap enforced; normal zero-tool-but-files and
  nonzero-tool exits unaffected. Full suite + fmt + clippy.

## E. RULES bump `.3` + docs

- [x] E1. `RULES_VERSION` → `2026-10-09.3`, history entry: "machine
  effort parity (opencode `--variant`, OMP attempt-fallback) + cutoff
  recovery (D). Prompt templates unchanged; archived rows comparable."
  Telemetry (A) and annotation (F) explicitly out of bump scope.
- [x] E2. `docs/DECISIONS.md` #9: harness-relative effort semantics,
  attempt-then-fallback on both harnesses, recovery-over-reclassification,
  2-recovery cap rationale.
- [x] E3. (fingerprint gap recorded in .3 history entry) Fingerprint note: composed prompts (prose + variant + flag)
  are not hashed — only templates are. Record the gap; bump carries it.

## F. Annotate archived cutoff trials (no rescoring, dry-run default)

- [x] F1. (26 trials flagged; exact 1:1 overlap with session-derived pure list) `annotate-cutoff [--yes]` CLI (repair/cost-annotate idiom):
  map the 26 pure-signature sessions (`user → reasoning-only
  finish:length → idle`, session id embeds run id) to trial manifests;
  set `cutoff_turn1: true` (new `Option<bool>`, serde default) on trial
  manifests + bench.db `runs` rows. Scores, costs, parents untouched.
- [x] F2. (backup bench.db.bak-cutoff-20261009; idempotent re-run = 0) Idempotence + dry-run-first verification; backup `bench.db`
  before `--yes` (precedent: `bench.db.bak-*`).
- [x] F3. (incl. billing-tag vs raw-shape exclusion; shared-tempdir flake fixed + documented) Unit tests: signature matcher (pure-cutoff vs cheat-DQ with
  tools vs genuine failure); null-vs-absent field handling.

## G. Deploy + effect check

- [x] G1. (release restart, integrity clean, idle before+after) [GATE: idle + 30s notice] Release build, restart
  engine+gui, integrity clean, `migrate-check` clean.
- [ ] G2. q51 RAN 14:54–15:14Z but verdict INCONCLUSIVE (see incident below).
  Confirmed in prod: variant `max` resolved (session records it, no
  fallback), telemetry live (turns 101 on API), recovery correctly silent
  (51 tool calls → trigger rightly not fired). Effect (reasoning volumes
  vs prose baseline) NOT established: trial's workspace was deleted
  mid-trial by an unidentified external actor, agent worked 101 turns in
  a detached mount + container /tmp, archive faithfully copied an empty
  dir → 0% stands per no-rescoring rule, flagged for rerun.
  INCIDENT 2026-10-09 ~15:05–15:13Z: staging workdir deleted mid-trial
  (agent-observed `total 0` + mountinfo `//deleted`; agent cleared —
  no workspace wipe in its tool calls, it debugged the mount for 5
  turns). Ruled out: our pipeline (prep once at start, no mid-trial
  delete path), archive (copies, never deletes), sdb-watch (read-only),
  my commands (all read-only in window), tests by me (none ran).
  Unresolved: another box actor (second live session on pts/2) vs
  unknown engine path. Mitigations shipped: `test_web_routes_lifecycle`
  #[ignore]d (it remove_dir_all's the REAL workspace/ root; comment
  admits prior destruction) — see H7. RECOMMEND: $0 rerun of this exact
  cell as G2-redo after confirming no test/cleanup runs during waves.
  UPDATE (effect analysis): q51 reasoning 24,366 sits at the median of 28
  prose-only redis/ling sessions (median 21,461, max 43,202) — n=1,
  indistinguishable. Resolution proven, effect NOT established.
  Discriminating test proposed: pdf/max/opencode single trial ($0; the
  cutoff-inducing task) — also exercises recovery live if cutoffs recur.
  Propose both reruns (redis verdict-integrity + pdf effect) with user
  go-ahead.
- [ ] G3. Node `--check` on GUI JS if touched; portal regen only if
  displayed numbers change (annotation alone doesn't).

## H. Rest of roadmap (consolidated, unchanged priority)

- [x] H1. Slice 3c evidence (2026-10-09, 3 archived-trial replays, all
  MATCH): Robust1 PASS 3/3, differential PASS 3/3, Robust2 FAIL 3/3 with
  identical signature (slow-frame `$-1` vs bulk foo) — systematic, not
  flake. Reading: Robust1 + differential are promotion candidates (need
  user threshold + multi-model rows — all 3 are muse-spark); Robust2
  needs a POLICY decision first (strict reference-parity vs lenient: is
  answering a partial frame with $-1 fail-worthy?). [GATE: user sets
  promotion threshold + Robust2 policy] then promote + bump.
- [ ] H2. Slice 3b label: roadmap checkbox says "(redis/http)", oracle
  is redis-only by design — one-line fix with next commit touching
  TODO_RIGOR.md.
- [ ] H3. Matrix 1b-parity follow-up: matrix tab ignores `boardTier`
  (leaderboard/portal/models filter; matrix mixes tiers). Decide:
  filter vs explicit "mixed" badge. Small, GUI-only.
- [ ] H4. [GATE: spend approval] Slice 4b breadth wave: 63-cell full
  (~$60, 2–3 wks) or 36-cell narrowed (redis/http/proxy/pubsub).
  Includes rerun of ruined pdf/max/opencode cell under new stamp —
  doubles as resume-recovery field test.
- [ ] H5. Slice 5 paper (blocked on H4): 5a taxonomy (cutoff trials now
  a first-class category via §F flags), 5b language analysis, 5c draft.
- [ ] H6. Process rules (add to TODO_RIGOR.md Rules): freeze graded
  files during waves (mid-wave edits caused q50's 4-hash fingerprint
  drift); no archived rescoring without review (already DECISIONS);
  no `cargo test` (un-ignored live-fire subset only) or manual
  workspace/runs cleanup during a live wave — q51's staging was deleted
  mid-trial by an unidentified actor (incident note under G2).
- [ ] H7. Test isolation proper fix: test AppState with temp dirs so
  `test_web_routes_lifecycle` can run un-ignored without touching real
  workspace/runs/results roots (currently #[ignore]d as a loaded gun).

Order: A → B+C+D (any order, one branch each or stacked) → E → F →
G1 → G2 → H (H1 anytime; H4 on approval).

## I. Fresh ideal wave (2026-10-09, RUNNING) + deep analysis plan

Pre-wave ops (all verified): full archive to
`~/bench-archive/2026-10-09-pre-wipe/` (db 84/519/613 identical, 613/613
manifests, 84/84 results, spot-reads pass); wiped bench.db + runs/ +
results/ to zero (migrate-check clean, API 0 rows); rebuilt release
(RULES .3 + effective-effort fields) and restarted engine+gui on an idle
engine with 30s notice. portal/ left live (public board keeps old data
until the wave lands — do NOT regen/push an empty board).

Wave spec (user-approved): `openrouter/inclusionai/ling-3.1-flash` × 7
tasks × {low,high,max} × {omp,opencode} × 9 trials = 42 cells / 378
trials, uniform (no extend-if-unstable), enqueued q1–q42 (~17:11Z).
  2026-10-10: REMOVED pending opencode proxy cells (q25/q32/q39) — proxy
  task is infra-capped this wave (prompt port contradiction + upstream
  publish decay; see proxy diagnosis). Saved ~27 trials of unscorable
  runs. Done OMP proxy cells (q4/q11/q18) kept as records, flagged for
  post-drain annotation + rerun under fixed prompt. Queue now 39 cells.
First wave on the new binary: machine variants, cutoff recovery,
turn telemetry, effective-effort recording, live robustness probes.
Model spend $0 (catalog-free); review judge (muse-spark default) ≈ $2
total across the wave. ETA ≈ 22h on 4 workers (@ ~14 min/trial).

Deep-analysis plan (runs when the wave drains):
- [ ] J1. Boards per (task, effort, harness) with Wilson CIs; IQR spreads.
- [ ] J2. Variant effect, properly powered: #max vs prose reasoning
  volumes + outcomes (n=9, not n=1); dose-response low/high/max.
- [ ] J3. Recovery incidence: cutoff count, recovery count, post-recovery
  scores; cutoff_turn1 rate by effort × harness.
- [ ] J4. Effective-vs-requested audit (first real data for the new field):
  OMP downgrades, OpenCode fallbacks, "mixed" parents.
- [ ] J5. Failure taxonomy (5a): stage-level failure profile, DQ/cheat
  count, packaging faults, infra incidents (incl. q51 workspace case).
- [ ] J6. Cost anatomy: $0 API + compute floor per cell; review total.
- [ ] J7. Cross-harness side-by-side (never pooled): same-cell deltas.
- [ ] J8. Probe data on fresh trials: Robust1/differential promotion
  evidence toward the 3c threshold; Robust2 policy input.
- [ ] J9. Regen + push portal on landed data; paper inputs (5b/5c).

## K. Cheat top-up + merge rule (user-approved 2026-10-10)

Policy: 8 honest trials are accepted; the cheat stays visible. Two
halves — operational top-ups (no code) + merge-rule change (code, .4).

### K1. Top-ups (operational, NO code, NO bump)
- Vehicle: existing `POST /api/runs/{id}/extend {trials, queue:true}`
  (trial numbering continues at trial10+, no collisions; re-merges on
  completion). Precedent: q43 extension.
- DONE 2026-10-10 (engine live, .3 binary): q43 pdf/low +1, q44 pdf/high
  +2, q45 pdf/max +2, q46 pubsub/low +1 = 6 trials, positions 13–16.
  Same stamp as siblings (no deploy between) → no mixed fingerprints.
- Rules: top-up count = cheat count, cap 2 attempts per cell per round;
  a replacement that cheats again is accepted (cell ranks on valid n).
  Proxy cells EXCLUDED (need prompt fix first; full reruns instead).
- Post-drain: scan all cells (incl. opencode half) for remaining cheat
  gaps; repeat once if needed.

### K2. Merge rule (code DONE, deploy pending top-up landing)

Implemented + committed under RULES .4 (uniform-valid aggregates,
status all-cheat-only, matrix DQ markers, portal tags + methodology,
merge tests, template alias-shielding + render audit, upstream health
gate, stage-label dedup). Awaiting: top-up trials land → deploy .4
(30s notice, idle) → backfill recompute → proxy rerun wave.
- `merge_subruns`: drop `disqualified_cheat` subruns from pass
  aggregates (rate, P@1, counts). All-cheat parent → status
  `disqualified_cheat` (excluded, as today). Else status from valid
  trials + new `disqualified_trials: [ids]` list on the row.
- Visibility (the cheat must read clearly, not vanish):
  - Matrix cell: `{p1}% n=<valid> DQ<cheats>` (e.g. `66.7% n=8 DQ1`)
    with tooltip naming trial + reason + "excluded from score,
    retained in record".
  - Run page: DQ sections keep badges + add "excluded from parent
    score" line.
  - Portal methodology: one sentence on cheat exclusion + counts.
- Existing merge-DQ-propagation tests updated (they pin old behavior).

### K3. Backfill (post-deploy .4)
- Recompute all parents under the new rule (repair/backfill path):
  topped-up cells merge to n=9 (+ archived DQ trials alongside);
  untopped cells show n=8+DQ markers. Scores change only by removing
  cheat trials from denominators — no invented data.

### .4 scope (one bump, one deploy, one backfill)
cheat-merge + visibility + proxy template fix + upstream health gate +
stage-prefix cosmetic. Trial rows keep stamping pre-bump versions until
deploy; merges recomputed after.

## L. AGENTS.md port contradiction (found 2026-10-11 verifying the review,
FIXED + DEPLOYED 2026-10-11)
- Both runners wrote AGENTS.md with canonical ports while the brief
  carried worker ports (live quote: "task brief says 8191, AGENTS.md says
  8091 — cover both"). Same defect class as the prompt contradiction, one
  file deeper. Fix: both composers render through alias-shielded
  templating (helper moved config.rs); per-task render audits green;
  RULES 2026-10-11.1. SHIPPED 2026-10-11 (~17:40Z idle window; integrity + migrate clean).
- Follow-up then: per-trial binding assessment (which port each proxy
  candidate bound — session tool calls), targeted reruns for 8091-bound
  trials only.

## M. Proxy wave #2 results + upstream verdict (2026-10-11)
- Scores: 4-5% -> 27-33% avg (template fix works: stage 3 framing now
  passes, candidates bind worker ports). P@1 still 0.0 everywhere.
- Health gate FIRED in 59/60 trials (detection works) but stages 1-2
  still fail 106/108: recreation does not help. Daemon logs show clean
  container starts, no proxy/iptables errors - bindings die between
  recreation and dial with no daemon-side trace. Manual repro of the
  exact setup works; in-network paths work (19/19 refs).
- Conclusion: host-published docker fixtures are unreliable at verify
  time on this box; retrying recreation is futile (already retried once
  per trial by the gate). Structural fix required.
- IMPLEMENTED + DEPLOYED 2026-10-11 (user go-ahead; idle window,
  integrity + migrate clean): stages 1-2 learn markers from identical
  in-process stubs on publish decay (docker path untouched when healthy;
  dead containers fail loud naming the fixture); stage 4 kills via
  pause/unpause (immune to stop/start re-publish + concurrent-rm races)
  with rejoin proven end-to-end via proxy (strictly stronger - slow
  rejoin now fails; documented). Bumped to 2026-10-11.2 anyway
  (consistency + visibility). Verified: marker parity, local-markers
  order, dead-fixture naming, live docker decay-simulation integration
  test (green), full suite 355/0, clippy clean.
- NEXT: proxy wave #3 for clean data (6 cells, fresh .2 rows).

## N. Fresh screening wave (2026-10-13, RUNNING) + reset record

Pre-wave ops (all verified): full archive to
`~/bench-archive/20261013-0645-pre-reset/` (db 50/431/482/58 identical,
482/482 manifests, 50/50 results, spot-reads pass); wiped bench.db +
runs/ + results/ to zero (migrate-check clean, API 0 rows, queue 0);
portal/ archived but left live (stale until new wave lands — do NOT
regen/push an empty board). Engine binary verified live: built after
all screening commits, serves 2026-10-12.3 + SCREEN_CAP/auto-extend
strings; no restart needed.

Wave spec (new method): stop-on-pass screening (cap 4, flat — no
escalation), narrow auto-extend MANUAL-only (envelope unset: auto fires
nothing until SDB_AUTO_EXTEND_TRIALS is approved), rank key unchanged
(P@1-lo) + capability co-column. Canary first: 3 models
(ling-3.1-flash baseline, inkling:free, nemotron:free — all $0/$0 in
live catalog) x redis x high x {omp, opencode} = 6 cells (q62-q67).
Requested --trials 9 clamps to 4 initial per cell. Expand to full
breadth on canary green.

## O. Capability-method canary (2026-10-13, RUNNING) + second reset record

Pre-wave ops: stopped the screening canary mid-wave (1 trial running,
5 done — archived first), full archive to
`~/bench-archive/20261013-0800-screening-canary/` (db 7/6/16/0,
16/16 manifests, 7/7 results, spot-reads pass); wiped bench.db +
runs/ + results/ to zero (migrate-check clean, API 0 rows, queue
cleaned to the 6 new cells); portal/ archived but left live (stale
until new data lands).

Wave spec (new method + score system): stop-on-pass screening (cap 4)
+ feedback repair ON (--feedback = 2 rounds, same workspace, shared
turn pool, failure diagnostics) + capability scoring (solved@N,
P@1-lo stays rank key, Cap co-column). Narrow auto-extend stays
manual-only (envelope unset). Canary: 3 models (ling baseline,
inkling:free, nemotron:free) x redis x high x {omp, opencode} = 6
cells (q68-q73), all verified stop_on_pass + fb=2 at enqueue.

## P. Scoring fix (2026-10-13, RULES 2026-10-13) — non-result exclusion + repair telemetry

Two live defects, both in the scoring path (`merge_subruns`), fixed in one
bundle with one RULES bump:

1. Non-result trials scored as 0% failures. `agent_failed`/`agent_killed`/
   `infra_error`/future statuses entered P@1 denominators and means.
   Live impact: nemotron/omp canary parent read n=4 P@1=0/4 (trial 4 was a
   billing-throttle `agent_failed`); honest is n=3 P@1=0/3. Archive impact
   (annotated, NOT rescored): two ling 9-trial cells read 33.3%/66.7%,
   honest 50%/75%. Fix: `is_valid_trial` (None legacy + completed +
   failed_tests) gates every aggregate; excluded trials stay in subruns;
   `solved_at` uses trial numbers; all-excluded cells carry a neutral
   non-ranked status; boards (leaderboard finalize + both portal filters +
   post-regroup) exclude non-rankable parents; fault pills render neutral.

2. Repair attempts invisible unless converted. Parent `repaired` read None
   for the omp canary cell despite 8 fired repair rounds. Fix: new
   `repair_attempted` (trial + parent + manifest + result rows); `repaired`
   keeps converted-only semantics; `feedback_rounds` documented as rounds
   used. Same bundle also defines the previously-undefined `dqMark` matrix
   badge (latent ReferenceError) and adds ⚠ fault badges + repair tooltips.

Cutover: live canary parents re-merged via `backfill --yes` (scores move
only on the omp cell; 12.3→13 re-stamp, graders preserved). Archives keep
their stamps per doctrine.
