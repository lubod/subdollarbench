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

- [ ] G1. [GATE: idle + 30s notice] Release build, restart
  engine+gui, integrity clean, `migrate-check` clean.
- [ ] G2. [GATE: micro-spend ~1 trial] Single max-effort OpenCode trial
  as canary: assert `--variant` accepted (no resolution error) and
  reasoning-token volume moves vs prose-only baseline. If forwarding is
  inert → B4 body-passthrough path before any wave.
- [ ] G3. Node `--check` on GUI JS if touched; portal regen only if
  displayed numbers change (annotation alone doesn't).

## H. Rest of roadmap (consolidated, unchanged priority)

- [ ] H1. Slice 3c: gather flake evidence free via `replay` on archived
  redis passes (probes run inside replay); [GATE: user sets promotion
  threshold — N clean rows across M candidates] then promote + bump.
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
  drift); no archived rescoring without review (already DECISIONS).

Order: A → B+C+D (any order, one branch each or stacked) → E → F →
G1 → G2 → H (H1 anytime; H4 on approval).
