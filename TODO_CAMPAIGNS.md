# Campaigns — next evolution roadmap (2026-10-14)

The bench becomes a **developer's week**: one living codebase, ordered
tickets, cumulative acceptance, cost-first scoring, runtime faults.
See `docs/PROPOSAL-iterative-campaigns.md` (ladder + contracts) and
`docs/PROPOSAL-scoring-v2.md` (protocol/curve/Pareto).

## sdb-sql career phases: four cells under $1 each (opened 2026-10-21)

**Operator decision (2026-10-21):** the 21-ticket ladder is split into
**4 phases**. Each phase is its own cell under the M5 cap ($0.99 / 150
min), run in **career mode**: phase N continues the same model's
workspace, data dir and ladder state from its phase N-1 cell. A model's
career score is the sum of its phase points (max 76). Reviewer: Claude
Code (REVIEW.md). Do the items in order, and **commit each item
separately** with its own rules bump.

| Phase | Tickets | Tiers | Max points | Ticket budgets (API) | Haiku 5.5 `.31` actual |
|---|---|---|---|---|---|
| 1. Core + durability | 0-6 | 1-2 | 10 | $0.97 | $0.49 |
| 2. Relational | 7-10 | 3 | 12 | $0.54 | $0.56 |
| 3. Engine | 11-15 | 4-5 | 22 | $0.79 | - |
| 4. Hard | 16-20 | 6-7 | 32 | $0.90 | - |

### P0. Cap enforcement (rules `.32`): prerequisite, nothing else is safe without it

The first Haiku 5.5 cell ended at **$1.0495 against $0.99**
(`runs/campaign_sdb-sql_openrouter_anthropic_claude-haiku-5.5_20261020_200329`).
A reviewer patch was lost in a reboot, so this is a spec to implement:

1. **One API budget per attempt.**
   - Today every repair round gets `max_budget_usd: Some(config.budget_usd)`
     again (`pipeline.rs`, round 0 and the repair loop). Ticket 9 metered
     $0.1307 of $0.12 in round 0 and then $0.055 more in its repair.
   - Add `cost_usd` (the live-metered API spend: OMP `accumulated_cost`,
     opencode Σ step cost) and `max_call_usd` (the largest single call) to
     `OmpSessionStats`. Sum them across rounds.
   - Add a pure `repair_round_spend(budget, api_spent, cap_left) ->
     Result<(Option<f64>, Option<f64>), String>`. A repair round gets
     `budget - spent` and does not start (log `[LIMIT] Repair round N not
     started: …`, and break *before* stopping the candidate or writing
     FEEDBACK_REPORT.md) when less than `BUDGET_FLOOR_USD` ($0.02) is
     left of the budget or of the cap.
2. **In-session stop at the cell's effective headroom.**
   - Add `AgentExecutionLimits::max_effective_usd: Option<f64>`. Check it
     in the runner's wait loop (OMP status loop, opencode `waited` loop),
     not per session line, because the time floor grows between calls.
   - Spend there = metered API + `calculate_effective_cost` floor of the
     elapsed seconds, fresh tokens and cache-read tokens. OMP must start
     counting cache reads (`accumulated_cached`).
   - On a trip, log `[LIMIT] Reached the campaign cap's headroom ($x / $y
     effective)` and stop like a budget trip (Ok, partial stats).
   - The pipeline passes, per round: `cap_headroom - (attempt API so far +
     compute floor of the pipeline's elapsed time and tokens so far)`.
3. **Reserve.**
   - `CAP_RESERVE_USD = 0.02`; `cap_reserve(max_call) =
     max(cell's largest call so far, 0.02)`. Haiku's largest was $0.033,
     DeepSeek's $0.0225.
   - `StageAttempt` / `StageJob` / `CampaignStageConfig` carry
     `cap_headroom_usd = cap - total - reserve` (INFINITY, or None when
     uncapped). `StageOutcome::max_call_usd` comes back via
     `PipelineOutput`.
   - A ticket starts only if `cap - total >= reserve + BUDGET_FLOOR_USD`;
     the lite check uses the reserve the full attempt just raised. The
     api_ratio clamp stays, applied to the net headroom.
   - Log one `[COST] metered … largest call …` line per attempt.
4. **Gates:**
   - unit tests for `repair_round_spend` (the $0.1307/$0.12 and
     $0.0364/$0.02 cases);
   - a fake-docker `cap` scenario in **both** harnesses (two calls, $0.01
     then $0.03, `max_effective_usd = 0.02`, $1.00 API budget: the cap line
     trips, never the budget line; stats carry `cost_usd` 0.04 and
     `max_call_usd` 0.03);
   - a ladder replay of the Haiku ending: cap $0.99, $0.9553 spent with
     the largest call $0.0333, so ticket 10 is **not** started;
   - a lite test showing the raised reserve skips a lite;
   - update `test_ladder_cap_converts_headroom_to_api_dollars` (first
     budget 0.045, cap headroom 0.18, ends as `cap_reached`).
5. **Score impact (put it in the rules history):** a ticket whose round 0
   spends its budget is no longer repaired. Haiku's t9 (3 points) and
   DeepSeek `.29`'s t19 (7 points) passed only that way.

### P1. Phases + career chaining (rules `.33`)

1. **Phase list in the spec:**
   - `campaigns/sdb-sql/campaign.json` gains
     `"phases": [{"id": 1, "title": "Core + durability", "stages": [0,1,2,3,4,5,6]}, …]`.
   - The validator checks that phases partition the stage ids, are
     contiguous and in order, and that no stage `needs` a stage from a
     *later* phase.
   - `max_points(phase)` = Σ tier over the phase's stages (10 / 12 / 22 /
     32).
2. **CLI:** `campaign run sdb-sql --phase N --chain <prev_campaign_id>`.
   - `--phase 1` needs no chain.
   - `--phase N>1` **requires** `--chain`. The previous cell must be
     phase N-1, same model, same harness and effort, same rules version,
     and finished. Otherwise refuse with the reason.
3. **Carry-over from the chained cell:**
   - the workspace (as-is, including `.git`, so the stage history
     continues), `sql-data` and `sql-data.snap`, the sentinels, and
     `baseline.json` (the perf gate's genesis baseline);
   - the ladder state: `outcomes`, `passed_all`, `liters` (lite profiles),
     `best` (for regression detection), `dirty`, and the last-green commit.
   - Persist that state in the cell dir (for example `ladder_state.json`
     written at every stage boundary) instead of re-deriving it from
     `campaign.json`.
4. **Rollup:**
   - `phase`, `chain_from`, `max_points` for the phase;
   - a `career` block: points so far, total spend across the chain, and
     the cell ids.
5. **Console:**
   - Launch form: a phase picker plus a "continue from" dropdown listing
     finished cells of the previous phase for that model.
   - Board: one row per model career, with per-phase points and cost
     columns and the career total. Group by rules version as today.
   - Run view: the chain links.
6. **`scripts/sql-cell.sh`:** add an optional `PHASE CHAIN` (keep the
   defaults $0.99 / 150).
7. **Gates:**
   - a spec-validation test (partition, no forward needs, points
     10+12+22+32 = 76);
   - a chain test with a fake exec: phase 1 fails t4, so phase 3's 11 and
     13 are recorded skipped and cost nothing, and phase 2 is untouched;
   - the refusal paths (wrong phase, other model, unfinished cell);
   - a carry-over test: phase 2's first attempt sees phase 1's `passed`,
     lite profiles and `rollback_first` state.

### P2. Career hints: keep a failure from sinking later phases (rules `.34`)

In career mode one red ticket compounds. Failing t4 blocks 5, 6, 11, 13,
16, 17 and 19, which is 30 of 76 points. Two mechanisms, both bounded by
the phase cap:

1. **Catch-up slot at phase start.**
   - Before phase N's first ticket, re-attempt each earlier *failed*
     ticket that a phase-N ticket `needs`. Run its lite variant if it has
     one, else the full ticket at its lite budget.
   - Order: by how many phase-N points it unblocks, then by id. At most
     2 catch-ups per phase.
   - A catch-up pass unblocks `needs`, but scores only the lite weight
     (0.4 x tier) and is recorded `status: "catch-up"`. Repeating cannot
     beat passing first time.
   - It runs with the normal SPEC/rollback/preamble machinery.
2. **Hints in the harness note (no points cost).** When a phase starts
   with failed or skipped tickets, the preamble adds per broken ticket:
   - its classified failure from the last attempt
     (`FailureSummary.line` plus the minimal repro already produced by
     the verifier);
   - the SPEC.md section that the ticket owns;
   - which phase-N tickets it blocks, and their points.

   The hint is written by the harness from recorded verifier output,
   never from a reference solution.
3. **Alternative to evaluate, not to build yet: "assisted unblock".**
   - If a catch-up also fails, the harness applies a reference patch for
     just that ticket (a verified per-ticket diff on top of the model's
     code). The ticket scores 0, and the career continues with its
     `needs` met.
   - It needs per-ticket reference patches that apply to arbitrary model
     code, which is likely infeasible in general. Write a half-page
     assessment in REVIEW-style before anyone builds it.
4. **Gates:**
   - catch-up ordering and the cap of 2;
   - a catch-up pass scores 0.4 x tier and unblocks;
   - a catch-up skipped when the reserve plus floor isn't left;
   - the hint preamble names the blocked tickets and their points, and
     includes the stored repro;
   - a replay of the `.29` DeepSeek ladder as a phase split: t8 and t11
     failures produce catch-ups at phase 2 and 3 starts.

### Rules for this work

- Each item is a rules bump with a history entry (what changed, why,
  score impact).
- Run the full `cargo test` niced, plus fmt and clippy. Do not restart
  the services and do not launch paid cells. The operator does that
  after review.
- Cells from before `.33` stay single-ladder rows under their own rules
  version.

### P0 landed (2026-10-21, rules `2026-10-19.32`)

The cap-enforcement prerequisite is implemented and committed separately:

- `OmpSessionStats` now carries `cost_usd` and `max_call_usd` (summed across
  rounds in the pipeline); the OMP tailer tracks cache reads and the largest
  call, and the OpenCode `step_finish` reader does the same.
- `AgentExecutionLimits.max_effective_usd` is checked in both runners' wait
  loops (OMP status loop, OpenCode `waited` loop): effective spend = metered
  API + the compute floor of elapsed time and fresh/cached tokens; the trip
  logs `[LIMIT] Reached the campaign cap's headroom ($x / $y effective)` and
  stops normally with partial stats.
- One API budget per attempt: `repair_round_spend` gives a repair only
  `budget - spent` and refuses to start below $0.02 of budget or cap; the
  guard runs before the candidate stop/FEEDBACK_REPORT.md.
- The cell cap reserves `max(largest call so far, $0.02)` and starts a
  ticket only when `cap - total >= reserve + $0.02`; `StageAttempt`,
  `StageJob` and `CampaignStageConfig` carry `cap_headroom_usd`, and
  `StageOutcome.max_call_usd` feeds the next reserve. The lite check uses
  the reserve the full attempt just raised; `api_ratio` applies to the net
  headroom.
- `[COST] metered $x, largest call $y` is logged once per attempt.
- Gates: unit tests for `repair_round_spend` ($0.1307/$0.12 and
  $0.0364/$0.02), `cap_reserve`/`cap_allows` (the Haiku $0.99 / $0.9553 /
  $0.0333 replay, and a lite skipped by the raised reserve), the updated
  headroom conversion test, and fake-docker `cap` scenarios in both
  harnesses: calls $0.01 then $0.03 against `max_effective_usd = $0.02` and
  a $1.00 API budget, the cap line trips (never the budget line) and the
  stats carry `cost_usd = $0.04` / `max_call_usd = $0.03`. fmt/clippy and
  the full suite are the shared gates below.
- Score impact (also in the rules history): a ticket whose round 0 spends
  its API budget is no longer repaired; Haiku 5.5 t9 and DeepSeek `.29` t19
  passed only that way. Services are not restarted and no cell is launched
  from this work.
- Gate evidence (2026-10-21): fmt/clippy clean; bins 60 passed; the focused
  P0 tests (campaign cap/repair/lite/headroom, both fake-docker `cap`
  scenarios) pass. The full serial suite is **970 passed / 8 failed / 6
  ignored**; all 8 are the memory-cap live proofs (SQL T16 and the T17-20
  cascade, plus the unrelated KV stage 14). The **same T16 proof fails
  identically at pre-P0 HEAD `9f2ff24`** in a scratch worktree: the
  reference is OOM-killed (exit 137) after ~100k rows under the 64 MiB
  cgroup, so this is a host/page-cache environment regression, not P0.
  The P0 code paths themselves are covered by the focused tests above.

### P1 landed (2026-10-21, rules `2026-10-19.33`)

Career phases and chaining are implemented:

- `campaign.json` now carries four phases: Core + durability (0-6),
  Relational (7-10), Engine (11-15) and Hard (16-20), with 10/12/22/32
  points (76 total). `validate_spec` checks the partition, ordering,
  uniqueness and that no stage needs a later phase.
- `campaign run --phase N --chain <cell>`: phase 1 needs no chain, later
  phases require one; the chain cell must be phase N-1, the same model,
  harness, effort and rules version, and finished. The previous workspace
  (including `.git`), `sql-data`, `sql-data.snap`, `sql-sentinels.snap`,
  sentinels and `baseline.json` are copied into the new cell, and the
  ladder state is loaded from `ladder_state.json`.
- Rollups carry `phase`, `chain_from`, the phase `max_points`, a `career`
  block (points, spend, cell ids) and the persisted ladder state. The
  pipeline's per-stage on_stage callback writes `ladder_state.json` at every
  boundary, including the last-green commit.
- Console: phase picker, continue-from datalist populated from finished
  previous-phase cells of the selected model, phase/chain in the resolved
  command and request, and phase/chain/career metadata in the Run view.
- `scripts/sql-cell.sh` accepts `SDB_CELL_PHASE` and `SDB_CELL_CHAIN`
  (defaults $0.99 / 150 unchanged); extra argv still passes through.
- Gates: spec partition/points test (10+12+22+32 = 76); phase filtering
  with a chain (phase 3 runs only stage 12; 11/13/14/15 skipped and cost 0;
  phase 2 untouched); carry-over (phase 2's first attempt sees phase 1's
  passed set, lite profile and dirty rollback); chain refusal paths (wrong
  phase, other model, unfinished, no state, harness-fault/non-result). The
  lib suite with the eight known memory-cap live proofs skipped is **975
  passed / 6 ignored / 8 filtered**; the skipped set is the P0/host
  memory failure, not P1. Bins **60**; fmt/clippy clean.

### P2 landed (2026-10-21, rules `2026-10-19.34`)

Career catch-up and hints are implemented:

- At a phase start, up to **two** earlier failed tickets that phase tickets
  need (directly or through their needs closure) are retried first, ordered
  by the phase points they unblock and then id. A catch-up uses the lite
  variant when one exists (else the full ticket at its budget), scores
  **0.4 x tier**, satisfies `needs`, and is recorded
  `status: "catch-up"`. It runs through the normal SPEC/rollback/preamble
  machinery; a catch-up whose reserve plus floor does not fit is skipped.
- The phase preamble adds a career hint for each recorded failed/skipped
  ticket that blocks a phase ticket: the last failure line, the stored
  repro/detail, the blocked phase tickets and points, and the SPEC.md
  section. Hints cost no points and never contain reference code. The
  assisted-unblock alternative is assessed (not built) in
  `docs/ASSESSMENT-assisted-unblock.md`.
- Gates: 50 campaign tests, including catch-up ordering (4/8/11 by blocked
  points), the cap of two, 0.4 x tier and the unblocked phase ticket, the
  no-headroom skip, the hint text/repro, and the `.29` replay (t8 at the
  phase-3 start, t11 at the phase-4 start). The lib suite with the eight
  known memory-cap live proofs skipped is **980 passed / 6 ignored / 8
  filtered**; bins **61**; fmt/clippy clean. The skipped proofs remain the
  host/environment failure from P0, not P2.

### Round-19 review fixes (2026-10-21, rules `2026-10-19.35`)

Applied in the reviewer's order:

- **Q1 (HIGH)**: catch-up selection is greedy over `Failed` *and* `Skipped`
  tickets whose own needs are met, recomputed after each slot. The common
  phase-1 ticket-4 failure (4 -> 5 -> 6) needs three slots; the P2 two-slot
  cap is raised to **three** (`CATCH_UP_SLOTS`). This is the operator-facing
  decision recorded in the rules history; a two-slot "lost career" policy
  would be a one-line change if preferred. The 4 -> 5 -> 6 test shows 12
  and 14 unblocked.
- **Q2 (MEDIUM)**: a catch-up target must have its own `needs` in
  `passed_all`; the loop no longer pays for ticket 5 on top of a failed 4.
- **Q3 (MEDIUM)**: a catch-up harness fault calls
  `stop_on_harness_fault`, snapshots the state and exits the phase; the
  main loop does not run. The reason carries the `ticket N:` prefix.
- **Q4 (MEDIUM)**: a catch-up's SPEC.md includes every passed ticket with
  the target last, and its acceptance is the declared list plus
  `passed_all`; a catch-up at 4 with 7-10 passed verifies 7-10.
- **Q5 (MEDIUM)**: the Board groups a career chain and ranks it on
  `career.points`/`career.total_cost_usd`, with the phase cells expanded;
  full-ladder cells keep a separate class. The public portal hides phase
  cells until it has a career row of its own.
- **Q6 (MEDIUM)**: a chain cell cannot be continued twice. The
  `chain_already_used` scan runs under the campaign lock; a finished,
  non-faulted cell already naming the predecessor in `chain_from` refuses
  the new chain (no best-of-k careers).
- **Q7 (LOW)**: `LadderState.last_failures` carries each broken ticket's
  last `FailureSummary` across phases; hints fall back to it when the
  immediate predecessor no longer has the record.
- **Q8 (LOW)**: the pipeline reserves $0.002 for the final verification's
  compute floor in `max_effective_usd`; the `[COST]` line remains the place
  to check the first phase-4 cell.
- **Q9 (LOW)**: the shown phase ceiling includes the catch-up's 0.4 x tier,
  so a full phase plus a catch-up cannot read above its max.
- **Nits**: the duplicate doc comment, the stray `too_many_arguments`
  allow, the misplaced launcher test, the `SPEC.md` heading, the stale
  "slots below" phrase and the test-only `cap_headroom` are fixed. The
  `$0` commit-message expansion is left (already on master).
- **Environment**: the eight memory-cap live proofs still fail on kernel
  `-38`; the reviewer reports phase 4 is not runnable as graded until the
  cgroup/page-cache issue is understood. Phases 1-3 are unaffected.
  No service was restarted and no cell was launched.

Gates: 54 campaign tests (including Q1's 4 -> 5 -> 6, Q2 eligibility, Q3
fault stop, Q4 acceptance/SPEC, Q7 carried failure, Q9 ceiling); the Q6
chain-reuse test; the portal phase-cell guard; lib suite with the eight
memory live proofs skipped: **984 passed / 6 ignored / 8 filtered**;
bins **62**; fmt/clippy clean.

### Round-20 work: the last fixes before the first phase cell (opened 2026-10-21)

**Operator decisions (2026-10-21):**

- **D1. Three catch-up slots: confirmed.** Keep `CATCH_UP_SLOTS = 3`.
  Change the rules-history wording from "operator-facing choice" to
  "operator decision 2026-10-21".
- **D2. A started successor uses up its predecessor.** A rerun needs an
  explicit `--refork "<reason>"`.
- **D3. No real phase cell runs until this work is reviewed.** The first
  phase-1 cell runs on `.36`. A chain needs the same rules version, so a
  phase 1 on `.35` could not be continued.

**The work, in this order: one commit, rules `2026-10-19.36`.**

1. **R1: cancel, crash and rerun (D2).**
   - **What counts:** `chain_already_used` counts **any** cell whose
     rollup (in `campaign.json` or `status.json`) names the predecessor
     in `chain_from`. That includes running, cancelled, crashed and
     unfinished cells. The only exception is a harness-faulted cell (a
     non-result).
   - **Fault at start:** a cell that dies before its first `on_stage` has
     no rollup yet. Write `chain_from` into `status.json` at cell
     creation (already done for the live fields) and read it from there
     too.
   - **The override:** add `campaign run … --refork "<reason>"`. It is
     allowed only with `--chain` and only when the chain is already used.
     It records `refork: {of: <previous successor id>, reason}` in the
     new rollup.
   - **Without it:** the error names the existing successor and suggests
     `--refork`.
   - **Board:** show a `refork` badge with the reason on the career row.
   - **Console:** give the launcher a "refork reason" field, shown only
     when the server answers that the chain is used. `POST
     /api/campaigns` accepts `refork`.
   - **Tests:**
     - a cancelled successor (`status.json` state `cancelled`, empty
       `finished_at`) blocks the chain;
     - a successor that is only in `status.json` blocks it;
     - a faulted successor alone does **not** block (assert this with no
       other child present; the current test can't tell);
     - `--refork` passes and is recorded;
     - `--refork` without `--chain` is refused.
2. **R2: the Board's career leaf.**
   - **Grouping:** don't group harness-faulted cells into a career.
   - **The leaf:** the finished cell in the group that no other cell in
     the group names as `chain_from`. On a tie (a refork), take the newest
     `started_at`.
   - **Everything else:** the cumulative points and cost come from that
     leaf. A refork's superseded cell is listed under the expanded rows
     as "superseded".
   - **A running phase cell:** list it under its root as a "running" row.
     It carries `chain_from` in its live rollup, so its root is the root
     of its predecessor.
   - **Test:** do it in Rust if the ordering lives in `board_order`.
     Otherwise add a small JS check that the existing web test harness
     runs. Fixture: phase 1, a faulted phase 2, and a phase-2 retry → the
     leaf is the retry.
3. **R3 (nit):** `render_spec_md_catch_up` should take `passed` only and
   label each ticket from `outcomes` (Full or Lite). Remove the redundant
   filter.

**Rules for this work:**

- Bump `RULES_VERSION` to `2026-10-19.36`, with a history line.
- Gates: a niced `cargo test` (the eight memory-cap live proofs may stay
  skipped and named), fmt and clippy.
- Do not restart services, rebuild `target/release` or launch any cell.
  The reviewer reviews first.

**Separate task (not part of `.36`, may run in parallel): the memory-cap
live proofs on kernel `7.0.0-38`.**

- **Background:** SQL T16, the T17-20 cascade and KV stage 14 fail. The
  reference is OOM-killed (exit 137) after about 100k rows under the
  64 MiB cgroup, also at `9f2ff24`. It started after the reboot from
  `7.0.0-31`.
- **Find out why:**
  - Compare `memory.stat` of the proof container (`file`, `anon`,
    `file_dirty`, `file_writeback`) during the proof.
  - Check whether page cache from the store's writes is now charged
    and not reclaimed before the OOM. cgroup v2 under the systemd
    driver; look at `memory.high`/`memory.max`, swap, and `oom.group`.
- **Fix only on the verifier side, never by loosening the ticket:** for
  example, `memory.high` slightly below `memory.max`, or an explicit
  fsync/`posix_fadvise(DONTNEED)` in the *harness's* reference adapter.
- **Write it down:** record findings here. Bump the rules only if the
  grading changes.
- **Until then:** phase 4 is not runnable as graded, and phases 1-3 are
  unaffected.

**After the review (operator go-ahead needed for each):**

1. Build `target/release`, then restart `subdollarbench-engine` and
   `subdollarbench-gui`.
2. **Phase-1 cell:** `SDB_CELL_PHASE=1 scripts/sql-cell.sh <model>`
   ($0.99 / 150 min).
   - Check the `[COST]` and `[LIMIT]` lines.
   - Check that `ladder_state.json` exists and the cell stays under $1.
3. **Phase 2:** `SDB_CELL_PHASE=2 SDB_CELL_CHAIN=<phase-1 id>
   scripts/sql-cell.sh <model>`.
4. **Phase 3**, which is the first live catch-up and hints. Phase 4 runs
   only after the memory proofs are green.

## sdb-sql ladder extension: tickets 12-20 (opened 2026-10-19)

Design of record: `docs/PROPOSAL-sdb-sql.md` §"The ladder" (rows 12-20) and
§"Stage acceptance details". Only stages 0-11 exist in
`campaigns/sdb-sql/campaign.json`; the validator already accepts ids 0..=20,
`stage_title()` names all 21, and the per-ticket gates are the same ones
0-11 went through (verifier + prompts + spec/acceptance, RULES bump and
history, the 0/3000 soak through the differential part, the 20/20 flake bar,
Docker live proofs, TODO record, board/portal check).

**Operator decision needed before ticket 16+ work:** the full 21-ticket
ladder is $3.20 / 680 turns (proposal line 181), while the cell policy is
$0.75 / 120 min. A $0.75 cell stops by "budget cap" around ticket 8-11 and
20 tickets at ~30-40 min each is 10-13 h of wall, so tickets 12-20 are
unreachable inside today's caps. Either add a "full-ladder" cell class
(e.g. $4-5 / 8-12 h) or accept the cap stop as the result; a cap stop is
never a model ceiling.

- [x] **T12. Concurrency and isolation** (tier 4, needs 1, 2, 3, 6, full + lite) —
  **landed 2026-10-19, rules `2026-10-19.18`**
  - `src/verifier/sql/concurrency.rs`: (a) lost update, 16 clients × 50
    read-modify-write transactions, final value exactly 800 (bounded retry
    on `ERR busy`); (b) transfers with three snapshot readers (the sum
    never changes, at least ten snapshots); (c) full only: with a write
    transaction open, a reader answers the pre-transaction value within
    1 s — blocking and `ERR busy` both fail. The lite profile
    `concurrency-global-lock` skips (c).
  - scenario-only stages: `driver::scenario_only`/`stage_supported`; the
    pipeline runs the scenario after the suite and records a late failure
    against stage 12; the campaign spec has the stage (acceptance `0..=12`,
    $0.20/40, lite $0.12/24 weight 0.4) and both prompts.
  - tests: eight mock-server tests (snapshot-reader mode passes all;
    global-lock passes a+b and fails (c); naive autocommit loses updates;
    busy retries succeed; five repeats), and the spec test (13 stages,
    tier 4, needs 1, 2, 3, 6, lite profile, max_points 30).
  - gates: fmt and clippy clean; lib **916 passed**, bins 60; the
    differential soak is unchanged (ticket 12 adds no grammar). Not yet: a
    live cell reaching ticket 12 (the cap decision above), and the 20/20
    manual repeat.
**Round-14 remainder (2026-10-19)**: P1 (portal and console medians use the
mean of the two middles for even counts: 7 + 13 -> 10.0, tested); N5
(feedback renders the candidate's raw error text, not just the class); the
sentinel compares `ERR` answers by class, because the N5 text made the
class-only stored expectations drift.

- [x] **T13. Schema change** (tier 4, needs 4; differential) — **landed
  2026-10-19, rules `2026-10-19.19`**
  - grammar: `ALTER TABLE ADD [COLUMN] [IF NOT EXISTS] name TYPE
    [DEFAULT lit]` (existing rows read the default), `DROP [COLUMN]
    [IF EXISTS]` (PK/UNIQUE/indexed refused as `constraint`, missing column
    `no_column`, a missing name under `IF EXISTS` is OK 0), `RENAME COLUMN
    old TO new`, `RENAME TO new` (taken names `exists`, unknown
    `no_column`/`no_table`); indexes and foreign keys follow renames; every
    change survives restart. `MAX_DIFFERENTIAL_STAGE` is 13.
  - the live schema matters: SQLite has no ALTER `IF [NOT] EXISTS`, so the
    oracle (Rust) emulates the existence check from `sqlite_master` /
    `pragma_table_info`, and the Python reference twin does the same (it
    also strips a trailing `;` before parsing — a stale `name;` token made
    one DROP silently no-op and was the only live-proof failure).
  - the toy engine (the soak/agreement reference) parses and executes all
    four forms, including the `if_[not]_exists` no-ops and the
    PK/UNIQUE/indexed drop refusals; names follow indexes and references.
  - spec: stage 13 ($0.12/30, tier 4, needs 4, no lite, acceptance
    `0..=13`) and the prompt; the ladder is 14 tickets (max_points 34).
  - gates: the toy↔oracle agreement and mutant sweeps pass with the new
    profile (129 `verifier::sql` tests); the reference passes every stage
    20×; the stage-13 live eval-only proof passes (14/14, stage-13 PASS,
    sentinel written); **0/3000 soak through stage 13 passes**
    (`clean_toy_soak`, 55.6 s) — it found two more bugs, both fixed and
    pinned: the toy skipped an `INSERT ... SELECT` width check when the
    SELECT had zero rows (SQLite checks it at prepare time), and the
    generator's "drop an indexed column" probe could pick a UNIQUE index
    whose create had failed, so the drop succeeded and the generator's
    schema model silently diverged (the probe now uses only non-unique
    indexes, guaranteed live).
  - round-15 fixes: SQLite's `cannot drop PRIMARY KEY column` and
    `cannot drop UNIQUE column` map to `constraint` (H1; the oracle and the
    Python twin), the soak now asserts `script.unknown.is_empty()` (0/3000
    passes again), and fresh rename/probe table names carry the seed
    namespace `s{seed:x}_t{n}` (H2), with the reference 20× test extended
    to stages 0..=13 on one store.
- [x] **T14. Observability** (tier 5, needs 6, 11) — **landed 2026-10-19,
  rules `2026-10-19.20`**
  - `observability.rs`: `EXPLAIN <select>` must name `INDEX
    <table>.<index>` for an indexed equality and `SCAN <table>` without
    one (and not both); `SHOW STATS` (name/value rows) must move exactly
    after a scripted mix of SELECT/INSERT/UPDATE/DELETE/BEGIN/COMMIT/
    ROLLBACK plus one failing SELECT — `statements` counts every line
    (the final SHOW included), kinds by first keyword even on failure,
    `errors` counts ERR answers, and the connection gauge follows opens and
    closes (a short retry absorbs TCP EOF latency).
  - scenario-only: `scenario_only(14)`, a pipeline late-failure check, the
    campaign stage ($0.12/24, tier 5, needs 6, 11, no lite, acceptance
    `0..=14`) and the prompt; the ladder is 15 tickets (max_points 39).
  - the Python reference answers both statements (SQLite's planner is
    translated into `INDEX t.ix`/`SCAN t`; the counters live in one
    locked server-wide table).
  - gates: four mock-server tests (faithful server, an index-ignoring plan
    and silent counters both fail, five repeats), the **stage-14 live
    eval-only proof** (15/15, `Stage 14: Observability -> PASS`), and the
    unchanged 0/3000 differential soak (stage 14 adds no grammar).
- [x] **T15. Tests and CI gate** (tier 5, needs 7, 8; lite 60 % coverage)
  — **landed 2026-10-19, rules `2026-10-19.21`**
  - `run_tests_gate_blocking` (pipeline.rs): the candidate's `./test.sh`
    runs twice in a sandbox container (`--network none`, 120 s cap each),
    then the coverage signal (Go measured via `go test -coverprofile`;
    other stacks self-report `coverage: NN.N%`, and the prompt says the
    number is not independently verified; round-15 M2), gate 80 % (lite
    60 %), then sensitivity: for each of tickets 7 and 8 the newest matching
    `stage N` commit has its own diff reversed with `git apply -R --3way`
    in a scratch copy that includes `.git`; if later work overlapped the
    same lines, the changed files are restored to their parent versions and
    the fallback is logged (round-15 M3). Before grading a red suite, a
    boot probe starts the reverted copy and requires a `SELECT 1` answer
    (round-16 L-a); a broken fallback is inconclusive. Either way
    `./test.sh` must go red.
  - scenario-only: `scenario_only(15)`; the stage-15 extra runs on later
    acceptances too (the persistent gate). Campaign stage: `$0.20/36`,
    tier 5, needs `[7, 8]`, lite `coverage-60` at `$0.12/22` (weight 0.4);
    full and lite prompts; the ladder is 16 tickets (max_points 44).
  - gates: the Docker fixture test (a green suite passes; a toothless suite
    fails "stayed green"; 65 % fails the 80 % gate but passes the lite one)
    and the **stage-15 live eval-only proof** (16/16, `Stage 15: Tests and
    CI gate -> PASS`, coverage 85 %, both sensitivity reverts logged).
    The full suite is green serialized (`cargo test --lib --bins --
    --test-threads=1`: 926 passed / 6 ignored, bins 60); under default
    parallelism the long Docker live proofs (stage 13/14) can exceed a
    start timeout and surface as `infra_error`, and each passes alone.
    fmt/clippy clean.
- [x] **T16. Larger than memory** (tier 6, needs 2, 4, 6, 7, 8, 11)
  — **landed 2026-10-19, rules `2026-10-19.22`**
  - `memory.rs`: scenario-only 16. The pipeline restarts the candidate on a
    fresh scratch `<data>-mem` with `--memory=64m --memory-swap=64m` (a
    hard cap: Docker's implicit equal swap is disabled; round-15 M1). The
    real store is
    restored afterwards, the scratch dropped), then the scenario creates
    `big` (400k rows, ~900-byte `t`, ~400 MiB on disk) and `dim` (1k rows),
    loads in 1k-row transactions, builds `big_k`, and checks COUNT/SUM/MIN/
    MAX, `ORDER BY t DESC, id DESC LIMIT 10` over the whole table, and the
    indexed join exactly. Expected values are computed while generating the
    rows (no second 400 MiB engine); the full config is pinned to the
    reference's prototype answers.
  - campaign stage ($0.20/40, tier 6, needs 2/4, no lite) + prompt; the
    ladder is 17 tickets (max_points 50).
  - gates: five mock-server tests (faithful, bad aggregate, bad order, bad
    join, brute-force top-10) and the **stage-16 live eval-only proof**
    (17/17, `Stage 16: Larger than memory -> PASS`, `[MEMORY] 400000 rows
    (~343 MiB of text) ... exact`; the reference stayed alive under 64 MiB,
    RSS ~17 MB). The full suite is green serialized (`--test-threads=1`:
    933 passed / 6 ignored, bins 60); fmt/clippy clean.
- [x] **T17. Incident: disk full** (tier 6, needs 5)
  — **landed 2026-10-19, rules `2026-10-19.23`**
  - `disk.rs`: scenario-only 17. The pipeline restarts the candidate on a
    fresh 16 MiB tmpfs at `/sdb-data` (no host bind), seeds 40 × 64 KiB
    rows, fills the volume via `docker exec dd` (a foreign `fill` file, to
    the byte), and requires a 64 KiB write to answer `ERR full` without
    being acked or applied, and every seed to read back byte-for-byte. The
    harness then removes the filler and the same write must succeed
    **without a restart** and read back exactly. A second write while full
    is not required (an engine with a preallocated journal may persist a
    small row legitimately; round-15 M4/LOW). The real store is restored
    afterwards.
  - campaign stage ($0.15/30, tier 6, needs 5, no lite) + prompt; ladder 18
    tickets (max_points 56).
  - gates: four mock-server tests (faithful, acked write, applied refused
    write, corrupted seed) and the **stage-17 live eval-only proof** (18/18,
    `Stage 17: Incident: disk full -> PASS`, `[DISK] seed 40 rows; ENOSPC
    refused the write and never applied it`, retry after freeing succeeds).
    Full suite green serialized (`--test-threads=1`: 938 passed /
    6 ignored, bins 60); fmt/clippy clean.
- [x] **T18. Hostile input and limits** (tier 6, needs 1, 6, 7, 8)
  — **landed 2026-10-19, rules `2026-10-19.25`**
  - `security.rs`: scenario-only 18. The pipeline restarts the candidate on
    a fresh scratch store with `SDB_SQL_PASSWORD` and `--pids-limit=1024`;
    a raw authenticated client checks: missing/wrong AUTH -> `ERR auth`
    with no password in the text; 60 deterministic byte-fuzz lines and 30
    grammar mutations survive; a 10,001-deep expression -> `ERR limit`; a
    16 MiB+1 statement -> `ERR limit` and the connection recovers; a
    four-way join with 1e9 matches is cancelled at ~5 s with `ERR limit`;
    a partial-line slow-loris does not delay another connection; the first
    256 held connections are accepted, the 257th is refused, an existing
    one answers, and the cap releases after closes. The pipeline also greps
    the candidate's container logs for the password.
  - the Python reference implements AUTH, the 16 MiB/nesting checks, a
    SQLite progress-handler 5 s cancellation enabled only by
    `SDB_SQL_STATEMENT_TIMEOUT_MS` (ticket 18's restart), slow-loris-safe threads and
    the 256-connection cap; the candidate launch gained `--pids-limit`.
  - campaign stage ($0.15/30, tier 6, needs 1, 6, 7, 8, no lite) + prompt; ladder 19
    tickets (max_points 62).
  - gates: five mock tests (faithful, no auth, password leak, no limits,
    uncapped) and the **stage-18 live eval-only proof** (19/19,
    `Stage 18: Hostile input and limits -> PASS`, `[SECURITY] auth
    enforced; ... 256-connection cap enforced`). Full suite serialized
    (`--test-threads=1`: 947 passed / 6 ignored, bins 60); fmt/clippy
    clean.
- [x] **T19. Replication and failover** (tier 7, needs 6; lite 2 nodes)
  — **landed 2026-10-20, rules `2026-10-19.26`**
  - `replication.rs`: scenario-only 19. The pipeline reuses the HA container
    plumbing (`start.sh --port/--data/--replicaof`, three nodes, one tmpfs
    per node) and exports `SDB_SQL_NODES=ha1:P1,ha2:P2,ha3:P3`. The
    scenario checks `SHOW ROLE` (`role`/`lsn`) on every node, catches the
    replicas up, checks `ERR readonly` writes against the existing table,
    freezes both replicas across the commit (a 1 s freeze; the ack must
    wait for a resume), waits for both to reach the committed LSN and
    still be replicas (round-18 N3), kills node 1, and requires exactly
    one survivor as primary within 10 s
    (equal LSNs, lower-port tie), every acked row on the winner, a new
    commit replicated to the other survivor,
    the old primary respawned as a replica (fresh tmpfs) converging and
    refusing writes, and the cumulative differential suite on the current
    primary.
  - lite (`two-nodes-promote`): two nodes, the replica frozen across the
    commit and caught up (round-18 N1), kill the primary, manual
    `PROMOTE`, no acked commit lost, the promoted node acks new commits
    locally (it is alone), cumulative suite on it.
  - the Python reference implements the retained log (`repl.log`), LSNs,
    sync-acked COMMITs (2 s), replica application through the same
    `answer` path (ALTER IF-exists emulation included), election, manual
    PROMOTE, and restart/fresh-rejoin. The candidate container launch gained
    `--pids-limit` (T18) and the HD node env/tmpfs variants. HA nodes use
    the worker's own `port+60..62` block; a cluster that cannot start is an
    infra fault, not a model failure.
  - campaign stage ($0.20/40, tier 7, needs 6, lite $0.12/24 weight 0.4) +
    full and lite prompts; ladder 20 tickets (max_points 69).
  - gates: six mock-cluster tests (faithful, stale replica, lost acked
    commit, missing election, writable replica, lite) and the **full and
    lite live eval-only proofs** (20/20 each, election/rejoin and
    PROMOTE/cumulative suite). Their full-suite gate is the shared serial
    run recorded with T20.
- [x] **T20. PostgreSQL wire protocol** (tier 7, needs 1, 6, 7; lite simple query)
  — **landed 2026-10-20, rules `2026-10-19.27`**
  - `pgwire.rs`: scenario-only 20. SSLRequest -> `N`; Startup (cleartext
    password when `SDB_SQL_PASSWORD` is set, AuthenticationOk,
    ParameterStatus `server_version`/`client_encoding=UTF8`,
    BackendKeyData, ReadyForQuery `I`); simple Query with
    RowDescription/DataRow/CommandComplete and `I`/`T`/`E`; SQLSTATE
    `42P01` (missing table), `42601` (syntax), `23505` (duplicate key,
    only when ticket 9 passed); a wrong-password Startup answers `28P01`;
    extended Parse/Bind/Describe/Execute/Sync with `SELECT $1` (text) in
    the full variant. The lite (`pg-simple`) grades the simple-query
    protocol only.
  - the Python reference gained a PG v3 listener on `--pg-port` (auth,
    ParameterStatus, simple/extended, SQLSTATE, transaction status); the
    pipeline restarts on a scratch store with `--pg-port` and a password,
    checks the container logs for the password, and runs a stock
    `psql` from `postgres:16-alpine` (`--network host`) as a cross-check;
    a missing image is an infra fault.
  - campaign stage ($0.20/40, tier 7, needs 1, 6, 7, lite $0.12/24 weight 0.4) +
    full and lite prompts; ladder 21 tickets (max_points 76).
  - gates: seven PG mock tests (faithful full/lite, SSL accepted, missing
    ParameterStatus, wrong SQLSTATE, broken extended, missing
    ReadyForQuery) and the **full and lite live proofs** (21/21 each,
    `[PG] ...`, `[PG] stock psql SELECT 1 -> 1`). The final full suite is
    the shared serial run below.
- Gates for tickets 19-20 (final full ladder): `cargo test --lib --bins
  -- --test-threads=1` green (**964 passed / 6 ignored, bins 60**);
  fmt/clippy clean; the dry-run shows 21 stages and the full $3.20
  budget.

### Round-15 review fixes (2026-10-19, rules `2026-10-19.24`)

Applied in the reviewer's order; no ladder change (18 tickets, max 56).

- **H1/H2 (ticket 13)**: PK/UNIQUE drop refusals now map to `constraint`
  in both the Rust oracle and the Python reference; the soak asserts no
  unmapped oracle messages (0/3000 green); rename targets and the
  missing-table probe use the seed namespace; the reference 20× suite now
  runs stages 0..=13 on one store (48 s).
- **M1 (ticket 16)**: `--memory-swap=<same>` accompanies every
  `extras.memory` cap; the stage-16 live proof passes with no swap.
- **M2/M3 (ticket 15)**: the prompt now says non-Go coverage is an
  unverified self-report; sensitivity reverse-applies only the ticket's
  own diff with `git apply -R --3way` (index stat cache refreshed), logs
  the whole-file fallback when later changes overlap, and the live proof
  exercises the narrow mode for both tickets 7 and 8.
- **M4 (ticket 17)**: both incident writes are 64 KiB (no statement-size
  contract is needed); a second write while full is no longer required,
  per the reviewer's LOW. The stage-17 live proof passes.
- **L1**: a scenario-only stage whose late check never ran now reports
  `not run: an earlier stage failed` (not `PASS`), and those results are
  not counted as regressions.
- **L2/L3/L5**: the observability baseline settles the connection gauge
  with two equal reads; `INDEX SCAN t.ix` is accepted as an index plan
  (mock test added); candidate error text in feedback is capped at 300
  chars (test added). **L4**: the ticket-12 (10 s) and ticket-16 (bounded
  heap) prompt wording now matches the harness. **L6**: no action.
- Not code: **M5** is the operator decision (a capped `.23` cell before a
  full-ladder class or accepting the cap stop); no cell was launched.
- Gates after the fixes: the soak 0/3000 with `script.unknown` asserted
  empty; the reference 20× through stage 13 on one store; the stage-15
  (narrow sensitivity), stage-16 (no-swap 64 MiB cap) and stage-17 (64 KiB
  writes) live proofs; full suite serialized (`--test-threads=1`): **941
  passed / 6 ignored, bins 60**; fmt/clippy clean. The reviewer's obsolete
  `rules-18` branch/worktree was removed.

### Round-16 review fixes (2026-10-20, rules `2026-10-19.28`)

Applied in the reviewer's order; the ladder is unchanged (21 tickets, max 76).

- **I1 (HIGH, ticket 19)**: the post-failover cumulative suite now calls
  `run_stage_with_variants` with the same `passed_lite` variants as the
  main suite, so a lite ticket 8 keeps `join-inner-only` after failover.
  A driver test runs 200 seeds with `(8, ["join-inner-only"])` and asserts
  no LEFT JOIN is generated.
- **I2 (MEDIUM, tickets 12/14/16/17/18/19/20)**: scenario grammar is now
  covered by each ticket's `needs`: 12 -> 1/2/3/6 (WHERE, ORDER BY, UPDATE,
  BEGIN/COMMIT); 14 -> 6/11; 16 -> 2/4/6/7/8/11; 18 -> 1/6/7/8 (SELECT 1 +
  transaction + aggregate/join for the four-way timeout workload); 20 ->
  1/6/7. 16/17/19/20 use plain DDL (no PK/NOT NULL) and whole-table reads;
  17 now sends only ticket-0 grammar (CREATE/INSERT/SELECT); 20 grades the
  23505 duplicate only when ticket 9's PRIMARY KEY passed. The proposal
  lists each scenario's statement kinds next to the needs table.
- **I3 (MEDIUM, tickets 11/16/18)**: the 5 s statement cancel is opt-in:
  only ticket 18's restart sets `SDB_SQL_STATEMENT_TIMEOUT_MS=5000`; the
  reference defaults to 0, so the long ticket-11/16 statements run without
  it. The T18 prompt states both the AUTH and timeout scopes.
- **I4 (MEDIUM, ticket 19)**: HA nodes use the worker's own block
  (`port+60..62`, no collision with the next worker); a cluster that
  cannot start sets `infra_error` instead of grading the model.
- **I5 (MEDIUM, ticket 19; superseded in `.29`)**: the `.28` commit killed
  the primary immediately after the COMMIT ack and derived the election
  from post-kill LSNs. Round 17 showed this cannot catch an asynchronous
  ack (the 88 ms kill lets it replicate) and can reject a correct
  higher-port winner after it ships its tail; `.29` replaces it with the
  replica freeze (round-17 J1). The prompt still says the old node rejoins
  on a fresh data dir.
- **L-a (ticket 15)**: before grading a red `./test.sh` after the
  sensitivity revert, a boot probe starts the reverted copy and requires a
  `SELECT 1` answer; a whole-file fallback that broke the server is now
  "inconclusive" instead of proof. The full stage-0/7 harness comparison is
  a known residual.
- **L-b (ticket 20)**: a wrong-password startup must answer `28P01`; a
  missing `postgres:16-alpine` is an infra fault, not a skip.
- **L-c**: T18 prompt AUTH/timeout scopes (above).
- **L-d**: `detect_regressions` skips `skipped due to failure in earlier
  stages` like `not run:`; the late checks of differential stages 5, 6 and
  11 now join the L1 sweep, so an unrun crash/txn/perf check cannot leave a
  placeholder PASS.
- **L-e**: the T14 gauge settle loop pauses 25 ms between reads.
- **Ops**: the manual experiment containers (rha1-rha3, refpg,
  refsec-bench) and the refha network were removed; no cell was launched.
- **M5** remains the operator decision (a capped `.28` cell vs a
  full-ladder class); no cell was launched.

Gates after the fixes: the affected mock/verifier suites and clippy are
green; the 0/3000 differential soak is green; the stage-15 (boot probe),
stage-19 full/lite (immediate-kill sync, superseded in `.29`) and
stage-20 full/lite live proofs pass; full serial suite: **965 passed /
6 ignored, bins 60**.

### Round-17 review fixes (2026-10-20, rules `2026-10-19.29`)

Applied in the reviewer's order; the ladder is unchanged (21 tickets, max 76).

- **J1 (HIGH, ticket 19)**: sync commit is tested with a frozen replica,
  not an immediate kill. The harness creates the table and seed rows,
  catches both replicas up, freezes them (`docker pause`), sends the
  transaction and `COMMIT` with a 1 s read deadline (an answer fails:
  "COMMIT answered within one second while every replica was frozen"),
  resumes one replica and requires the pending `COMMIT` to answer within
  5 s, then resumes the other. Both replicas reach the committed LSN
  (equal), so the lower port must win the election deterministically. The
  lite freezes its one replica, resumes it, then kills and `PROMOTE`s.
  `HaAction::Pause`/`Resume` map to `docker pause`/`unpause`; the mock
  models per-node pause flags, an asynchronous-ack mode that must fail and
  the same commit gate; the reference's election threshold is raised from
  1.5 s to 3 s (a 1 s freeze must not elect). Both prompts document the
  freeze, and the module doc/proposal/rules history are corrected.
- **J2 (MEDIUM, ticket 19)**: the read-only refusal now runs only after
  the table exists and both replicas have caught up, so a table-first
  engine answers `readonly` (the mock's `TableFirst` mode passes the full
  and lite scenarios).
- **J3 (MEDIUM, ticket 15)**: the boot probe polls for 25 s (500 x 50 ms),
  sets a 10 s socket timeout after connecting, and the
  `run_workspace_command` cap is 45 s.
- **K1 (ticket 20)**: the wrong-password probe bounds the message length
  to 4..=64 MiB and reports a post-password `AuthenticationOk` as
  "accepted the wrong password".
- **K2**: negative tests added: a PG mock that accepts any password (fails
  with the new message); `unique_keys = false` does not send the 23505
  probe (asserted with the mock's `duplicate_seen`); asynchronous
  replication is caught; a table-first replica passes full and lite.
- **K3/T6**: the round-16 I1 test is back above its own doc comment and the
  old persistence test regained its comment; the suite variants are
  extracted into `suite_variants` and unit-tested; late-check skips read
  `not run: late checks skipped after stage N failed`; the README lists
  `postgres:16-alpine` as a prerequisite and the build step pulls it.
- **Ops**: no cell was launched; cells stay on hold until `.29` is
  reviewed.

Gates after the fixes: affected verifier/mock suites and clippy green; the
stage-19 full/lite and stage-20 full/lite live proofs on the freeze test;
full serial suite **971 passed / 6 ignored, bins 60**. The first serial
run hit one unrelated timing flake in
`verifier::sql::concurrency::tests::the_scenario_repeats_cleanly` (7
snapshots vs the expected 10 under load); that test then passed 20/20 in
isolation and the full rerun was green.

### Round-18 review fixes (2026-10-20, rules `2026-10-19.30`)

Prepared by the reviewer in a separate worktree while the `.29`
calibration cell ran; the ladder is unchanged (21 tickets, max 76).

- **N1 (ticket 19 lite)**: after the frozen `COMMIT` is acked, the
  harness waits (up to the election budget) for the replica's `lsn` to
  reach the commit instead of sampling it once. An engine that acks on
  durable receipt and applies a moment later is correct; the mock's
  `ApplyLag` mode passes full and lite.
- **N2 (ticket 19)**: a failed pause, resume or kill on a node whose
  container is still up (or cannot be inspected), and any failed rejoin,
  is an infra fault (`[INFRA] REPL cluster control failed`). A node that
  already exited stays a model failure (`repl_control_failure_is_infra`,
  unit-tested).
- **N3 (ticket 19)**: after the freeze both replicas must still report
  `replica` ("the freeze triggered an election"; the mock's
  `ElectsOnFreeze` mode, which used to pass when the lower port elected
  itself). Both prompts state the freeze (processes stop, connections stay
  open, up to about 1.5 s), that `COMMIT` blocks with neither `OK` nor
  `ERR`, a 3 s failure-detection floor (full), and that `lsn` grows only
  when a write commits.
- **N4 (ticket 12)**: transfer writers pause 1 ms after each transfer, so
  the readers' 10 snapshots overlap the transfers on any engine; this is
  the race behind the round-17 gate flake in
  `the_scenario_repeats_cleanly`.
- **N5**: late-check skips name a failed late check as well as a failed
  differential stage; the sentinel builds its variants with
  `suite_variants`; the proposal's ticket-19 ports are `+60..62`. An
  extra lite async-ack mock test was added.
- **N6 (failure classifier, found in the `.29` calibration cell)**:
  `classify_failure` treats a late check the pipeline did not run ("not
  run: ...") like a verifier skip. Since `.28` the late-checked stages 5
  and 6 read "not run" whenever a later stage fails, and the classifier
  blamed them: the cell's failed rounds of tickets 6, 8, 9, 11, 12 and 13
  read "broke tickets 5,6: not run: ..." with kind `regression`, and the
  board showed that as the cause of tickets 8, 9 and 11. Grading was not
  affected (`detect_regressions` already skipped them).

Gates after the fixes (run after the calibration cell ended): fmt/clippy
clean; full suite **975 passed / 6 ignored, bins 60**, including the
stage-13..20 live proofs (19 full/lite on the freeze plus the role check)
and both sensitivity end-to-end tests; `the_scenario_repeats_cleanly`
20/20; the reviewer's live freeze probe on `.30`: sync 3/3 full and 3/3
lite, an asynchronous ack caught 4/4. The transfer check's snapshot count
on the in-process mocks rose from min 7 / median 23 to min 121 / median
125 (30 runs each).

### M5 decided (2026-10-20, rules `2026-10-19.31`): the cell cap is $0.99 / 150 min

- **Decision (operator):** raise the cap from $0.75 / 120 min to
  **$0.99 / 150 min**, the highest budget that keeps every cell under $1
  (the README's "Under-$1" claim). The console launcher, the engine's
  `POST /api/campaigns` default and `scripts/sql-cell.sh` all default to
  `campaign::CELL_BUDGET_USD` / `CELL_WALL_MIN`.
- **Evidence:** the `.29` calibration cell (DeepSeek v4.1 flash, omp/high,
  `runs/campaign-launch-20261020_154206.log`) reached ticket 19 with 34.6
  of 76 points and spent $0.748, hitting the cap after 71 of 120 min.
  Tickets 14, 15, 16 and 18 were skipped by needs (8 and 11 failed) and
  20 was not attempted. Spend ran at 32% of the attempted tickets'
  budgets, so a full ladder costs about $1.03 at that rate: a model of
  this class gets close to the top under $0.99, and reaching it stays
  part of the score.
- **Comparability:** the grading code is unchanged, but the board ranks
  by points within the current rules, so the cap change gets its own
  rules version; cells under the old cap stay readable under their own
  stamp.

## Baseline evidence — canary wave under 2026-10-14.1

First coherent dataset after the review fixes (6 cells, redis, high,
stop-on-pass, feedback=2, free models):

| model | harness | n | P@1 | GET tp | ref | drift | eff $ |
|---|---|---|---|---|---|---|---|
| ling-3.1-flash | omp | 1 | 100 | 130,548 | 250,000 | clean | 0.1403 |
| ling-3.1-flash | opencode | 1 | 100 | 107,759 | 271,739 | flagged | 0.1194 |
| inkling:free | omp | 2 | 50 | 189,394 | 257,069 | clean | 0.0445 |
| inkling:free | opencode | 1 | 100 | 137,552 | 276,243 | clean | **0.0185** |
| nemotron:free | omp | 2 | 0 | -- | -- | -- | 0.0565 |
| nemotron:free | opencode | 4 | 0 | -- | -- | flagged | 0.0901 |

What it proves: the instrument is stable (refs 250-276k; every passing
cell measured through the new GET window where the broken build measured
nothing); drift quarantine fired on 2/6 brackets; non-result machinery
worked live (agent_failed x2 + disqualified_cheat x1 excluded from n);
equivalent 100% passes cost **7.6x apart** ($0.0185 vs $0.1403) — the
cost-first thesis has signal.

## Decisions taken (operator)

1. Green field, from scratch, **stdlib only** (no frameworks, no RESP/DB
   libraries, no prebuilt daemons).
2. **11 stages** (0..10), cumulative acceptance; regressions cardinal.
3. **Runtime fault injection** in scope (signals, filesystem, cgroups,
   traffic, partition).
4. **Cost is the headline** (regression-free tickets per $); perf is a
   gate, never a multiplier.
5. **Tests are a deliverable**: 80% coverage enforced with a sensitivity
   check (revert sentinel -> suite must go red).
6. Dashboard/UI stage + **3-node HA finale** (auto-failover contract).
7. **Campaign portfolio (compose, don't 1:1)**: single-shot tasks retire to
   genesis/history; campaigns absorb them as capability stages. See the
   portfolio table below.

## Slice A — campaign core

Shipped 2026-10-14 (`src/campaign.rs`, `main_campaign.rs`, pipeline
plumbing; suite 382/0): spec+loader+validation, stage loop with workspace
continuity, per-stage prompt override, acceptance-subset verdicts,
regression detection with severity weights, git-per-stage (commit +
patch + stat), rollup + `sdb campaign {list,dry-run,run}` with
`over_budget`. Open: the live canary gate below (needs spend) and
non-redis acceptance adapters.

- [x] A1. `src/campaign.rs`: CampaignSpec/StageSpec (versioned serde),
  loader + validation (dense ids, prompts exist, acceptance known),
  malformed-spec tests. `campaigns/sdb-kv/campaign.json` is the fixture.
- [x] A2. Stage loop on `BenchmarkPipeline`: workspace continuity (skip
  wipe on stage>0), per-stage prompt override (campaign path), READY
  section reused, run_id `<campaign>_s<N>`, per-stage budget/turn caps.
- [x] A3. Cumulative acceptance: verifier stage-subset filter (redis
  stages by id); stage verdict = suites 0..i; regression detection with
  severity weights (genesis core > feature > metrics). Other tasks: later.
- [x] A4. `git`-per-stage: init after S0 verification; commit after each
  stage (`stage N: <title> [verdict]`); diff patch + stat archived per
  stage; churn on the rollup; judge reads diffs (wiring later).
- [x] A5. Rollup + CLI: `runs/<campaign>/campaign.json` (per-stage
  verdict/cost/turns/duration; regression-free tickets / $; survival
  vector; `reached`); `sdb campaign {list,dry-run,run}`; stop-on-budget
  records `over_budget`, never a model failure.
- [x] A6. Tests: mock-executor orchestrator tests; loader/validation;
  git helper; e2e eval-only CLI smoke (evidence tree verified).
- [x] Gate, part 1 (no spend): `--workspace` seed + full-ladder eval-only
  replay of the archived ling Go candidate — 4 tickets passed
  cumulatively (3/3 -> 4/4 -> 5/5 -> 6/6), non-accepted suites excluded,
  destructive AOF kill only at s3, git commits + patches + rollup
  (reached 3, tickets/$ computed). Regression logic proven by
  mock-executor tests (pass->fail flips, fault status, budget stop).
- [x] Gate, part 2 (spend) — **regression alarm proven live** (2026-10-14,
  ling, stages 0-1): ticket 0 PASS (3/3, $0.0610, 40 turns, git e46ed9d);
  ticket 1 FAILED with real regressions: the model exhausted its 25-turn
  pool mid-refactor, its last-minute full-file rewrite shipped a compile
  error (`ok declared and not used`), the server never started, and
  [1, 2, 4] were flagged as regressions (sev 3); the failing state is
  committed as ef63f1b [fail]. Ladder stopped at reached=0. Rollup:
  $0.1095, regression-free 1, tickets/$ 9.1. Fail taxonomy for this
  canary: budget-exhaustion + no-compile, not wrong expiry semantics.
- Calibration finding for D6: `max_turns` IS the shared pool (initial
  cap = min(max_turns, 50) with feedback on), so a 25-turn ticket leaves
  zero repair headroom — the repair round never ran ("Global turn budget
  reached across attempts"). Re-run after D6 is settled.
- Gate, part 3 — D6(b) validated live (2026-10-14, ling, run 2): all four
  tickets PASS — s0 3/3 (30 turns), s1 4/4 (19), s2 5/5 (19), s3 6/6
  (23 initial + 7 repair = 30). Reached 3, regression-free 4, $0.1743,
  tickets/$ 22.9. The repair round converted a real failure: ticket 3's
  initial attempt hit the cap mid-WAL with three comment lines mangled by
  the edit tool (compile error, 0/6 verification), and the 7-turn repair
  fixed exactly those lines -> 6/6. Candidate measured 108k GET req/s;
  write probe 816 SET/s.
- **D6(b) amendment (2026-10-14, later): repair floor 8 -> 12.** After
  three more cells whose repair rounds could not recover an interrupted
  refactor (ling run 4: fixed two compile errors then hit the limit
  mid-fix; ling run 5: surveyed 7 turns, one edit at the cap; inkling
  probe: never edited), the reserve is now `max(max_turns/4, 12)` capped
  at `max_turns/2`. Initial caps: 40->28, 30->18, 25->13; the half-pool
  cap keeps tiny pools sane (10->5, 4->2, 1->1). RULES `2026-10-14.6`;
  DECISIONS #12
  amended; `initial_turn_cap` tests updated. Watch for small tickets that
  used to pass initially now needing their repair round.
- Finding (Slice F dependency): campaign stages run WITHOUT a batch
  reference, so each passing stage tries the live per-trial ref — at s3
  the ref measurement failed ("Could not connect to ref-redis") and the
  speed reading went unscored. The perf gate needs a shared/batch
  denominator for campaign tickets (or a readiness wait + retry).

## Slice B — stage suites (expiry, pubsub)

- [ ] B1. Expiry suite: TTL family semantics (SET clears TTL, INCR
  preserves, negative/zero deletes, PTTL rounding, GETEX options).
- [ ] B2. Pub/sub suite: seeded channels, multi-channel subscribe,
  unsubscribe silence, publish ordering, receiver counts.
- [ ] B3. Stage-tag the existing redis verifier tests so cumulative
  selection is data, not code.

## Slice C — durability, incidents, observability

- [ ] C1. AOF acceptance: stage 6 covers fsync-before-ack + SIGKILL +
  replay on live tickets; torn-tail and replay-idempotence checks still
  to add (reuse WAL verifier patterns).
- [x] C2. ENOSPC injector (2026-10-14.3, v1): ticket 4 (acceptance
  [1,2,3,4,5,6,8]) adds the `SDB_DATA_DIR` contract; the pipeline restarts
  the candidate with `--tmpfs /data:size=8m` + `SDB_DATA_DIR=/data`
  (`start_candidate_enospc`), fills it with 240KB protocol writes, and
  stage 8 requires: acked data still reads, PING alive, the first
  unpersistable write returns an error and is NOT applied, later writes
  keep answering. Mock tests cover pass + never-full. Deferred sub-checks
  (documented SKIPs): free-space recovery and replay-after-incident need
  to free/reach the container tmpfs -> docker exec (shell must exist) or
  a privileged loop-mount injector; revisit before the perf-gate wave.
  Ticket 5's acceptance now also includes 8 (cumulative incident check).
- [x] C3. INFO suite (2026-10-14.2): verifier stage 7 — required fields,
  integer parse, monotonicity, randomized delta checks (hits/misses/
  commands/connections/expired). Runs ONLY when a campaign ticket
  explicitly accepts id 7; single-shot totals unchanged. Secret-in-logs
  grep lives in E3.
- [x] C4. `parse_info` + delta checks are built into stage 7; candidate
  log-line parsing deferred to E3 (pipeline already captures logs).

## Slice D — tests stage (coverage with teeth)

- [x] D1. `./test.sh` contract (2026-10-14.4): offline (network-none
  container), deterministic (two runs), <=3min per run, exit 0 = green.
- [x] D2. Coverage adapters: Go measured by the harness
  (`go test -coverprofile` + `go tool cover -func`); other languages read
  a self-reported `coverage.txt` ("coverage: NN.N%") — sensitivity check
  carries the anti-fake weight there (documented limitation).
- [x] D3. Sensitivity checker: scratch copy (no .git), stage-1 expiry
  diff reversed from the stage-0 commit, `./test.sh` must fail; compile
  failure counts as caught.
- [x] D4. Persistent gate: ticket 6 ships `./test.sh`; every later ticket's
  acceptance includes id 9 (ticket 6 = [1..9]). Ladder entries for 7+ must
  carry 9 in their acceptance list.
- [ ] D5. Sandbox image: python `coverage` preinstall and/or
  `cargo-llvm-cov` remain optional (only if a future grid model needs a
  real adapter for its language).

## Slice E — dashboard + security

- [ ] E1. Dashboard probes: `/` offline HTML (no external refs),
  `/api/stats` mirrors INFO under randomized traffic, `/events` SSE
  frame <2s, full RESP suite green while a dashboard stream is open.
- [ ] E2. Security suite: AUTH on RESP and dashboard, 8MB argument cap
  rejected before allocation, fuzz corpus (binproto/robustness seeds),
  slow-loris isolation + idle-incomplete timeout, 64MB cgroup.
- [ ] E3. Secret hygiene: password absent from logs and error strings.

## Slice F — perf gate + HA finale

- [ ] F1. Perf gate: RESP pipelining in order; >=1.3x own stage-0
  baseline under pipelined load; p99 budget; BenchLock discipline.
  PREREQ (canary finding): campaign stages must share a batch reference
  or retry the live ref — s3's ref measurement failed ("Could not connect
  to ref-redis") and the speed reading went unscored.
- [ ] F2. Multi-instance launch: N nodes, per-node port/data dir,
  readiness per node, worker port offsets.
- [ ] F3. HA acceptance: `ROLE` contract, replication bound (<=3s),
  **sync-ack zero acked-write loss**, SIGKILL master -> exactly one new
  master, old master rejoins as replica, cumulative suite green on the
  current master.
- [ ] F4. Partition injector (isolation), split-brain rules, rejoin
  convergence (stretch: own incident stage).
- [ ] F5. HA budgets: this stage is expensive; survival scoring prices
  it, do not hide it.

## Slice G — scoring v2 + portal

- [ ] G1. Protocol gating: rank only within (task, rules_version,
  harness, protocol); matrix uniform-protocol rule; row badges.
- [ ] G2. Decide SPRT(0.3,0.8) vs confirmatory top-up (simulation table
  in PROPOSAL-scoring-v2).
- [ ] G3. Throughput curve decision: keep / cap 1.5 / linear /
  report-only.
- [ ] G4. Pareto frontier: pass@1 (Wilson) vs $/solve; floor sensitivity
  0.5x/1x/2x from archived rows; display-only first.
- [ ] G5. Campaign portal view: survival curve P(stage>=i), cumulative
  cost curve, per-stage chips, regression badges, churn.

## Slice H — validation + paper inputs

- [ ] H1. Canary: one cheap model, full 11-stage ladder; publish the
  cost + survival curves.
- [ ] H2. Grid: 4-6 cheap models x sdb-kv; uniform effort/harness; one
  campaign per wave; never mix boards.
- [ ] H3. Failure taxonomy from campaign stages (truncation, tool
  misuse, regression patterns, HA failures).
- [ ] H4. Paper inputs: survival curves, $/ticket, harness deltas,
  regression-free rates.

## Slice I — stage-2 wave: disk residency, compaction, admin protocol

Ladder extended beyond the 11-stage v1 (tickets 11-13; stages 14-16;
RULES `2026-10-15.1`). The store's story continues: the dataset outgrew
RAM, disk filled with append-only history, ops tooling needs a machine
interface.

- [x] I1. Ticket 11 — disk-resident store (stage 14): pipeline restarts
  the candidate with `SDB_DATA_DIR` on a host bind mount under a 192MB
  memory cap; the verifier writes ~2000 x 240KiB values (~480MB) through
  RESP, spot-checks 40 full values byte-for-byte, deletes five keys, then
  the pipeline SIGKILLs + restarts and re-checks. A map-in-RAM candidate
  is OOM-killed. Prompt: `stages/11_diskresid/prompt.md`.
- [x] I2. Ticket 12 — compaction (stage 15): churn of 600 x 240KiB x 6
  (~860MB of records) then 400 deletes (48MiB live); the harness measures
  the host bind mount and requires the footprint <= 3x live bytes (144MiB)
  within 30s, survivors byte-exact, then SIGKILL + restart keeps survivors
  and deletions. Values are hex-of-PRNG (poorly compressible) so a
  compression-only design cannot fake the gate.
- [x] I3. Ticket 13 — admin binary protocol (stage 16): SDB1 v2 on port
  6395 (worker offset), published only when a ticket's acceptance includes
  stage 16. Framing fidelity (bad magic closes, PING/ECHO byte fidelity,
  corrupt CRC -> ERR_BAD_CHECKSUM with the connection kept open, unknown
  opcode -> ERR_UNKNOWN_OP, 50-frame pipeline with matched ids+payloads)
  plus v2 additions: the SAME keyspace as RESP in both directions and
  STATS (opcode 0x06) mirroring the INFO counters.
- [x] I4. Harness: `CandidateRunExtras.binds` (host bind mounts),
  `disk_bytes` footprint walk, seeded big-value helpers, sectioned admin
  stage (a monolithic async fn overflowed 2MiB test stacks in debug; the
  64KiB read buffer in `Conn::read_frames` is heap-allocated now), live
  docker bind round-trip covered in `live_docker_cov_tests`.
- [x] I5. Mock/offline validation: admin stage against faithful SDB1+RESP
  mocks (pass, split-store fail, missing-STATS-field fail); compaction
  gate pass (sparse log shrinks) and loud no-shrink fail; helper/dir-bytes
  unit tests; extras args bind assertion.
- [x] I6a. Eval-only LIVE proof (2026-10-15, no model): a reference mock
  candidate (offset index + append log + idle compaction + SDB1 v2)
  driven through the real pipeline for stages 14-16 — all three PASS
  (`test_pipeline_eval_only_stage2_disk_and_admin_live`, ~110s incl. the
  100%-pass bench): host-bind restart under the 192MB cap, ~480MB
  written/re-read byte-exact across SIGKILL, ~860MB churn reclaimed to
  <=3x live, admin channel with shared keyspace + STATS. The admin port
  for eval-only workers is passed via argv (real worker-0 campaigns use
  6395, matching the published 1:1 port).
- [ ] I6b. Model-driven live execution of stages 14-16 (pending a model
  that reaches ticket 11; the grid currently tops out at ticket 4).

## Slice J — sdb-edge (proxy campaign: routing / resilience / health / cache / limits)

Second campaign portfolio entry (DECISIONS #11 porting order), task
`proxy`, RULES `2026-10-15.2`.

- [x] J1. Task adapter: `campaign.rs` acceptance is per-task now
  (`redis_stages` / `proxy_stages`), the validator range is per task
  (redis 1..=16, proxy 1..=9), and the CLI resolves
  `stage.acceptance.stages_for(task)`.
- [x] J2. Ladder: 7 tickets on one living workspace — t0 genesis (route
  `GET /`, body/status fidelity, framing, X-Forwarded-For), t1 balance +
  keep-alive ([1,2,3]), t2 survive upstream loss ([1-4]), t3 fail fast
  under hangs ([1-4,6]), t4 active health + `/status` ([1-4,6,7]),
  t5 caching ([1-4,6,7,8]), t6 limits under a 128MB cap
  ([1-4,6,7,8,9]). Throughput (stage 5) stays single-shot for now.
- [x] J3. New stages 6-9 in `verifier/proxy.rs` (campaign-only: never run
  for single-shot proxy rows, so that board is unchanged): quick failover
  under a frozen upstream (2500ms per-request budget + retry + rejoin),
  `/status` health surface driven by active probes (flip while idle,
  recovery <=5s), TTL cache (`SDB_PROXY_CACHE_TTL`,
  `X-Cache: MISS|HIT`, `Age`, byte-identical bodies, expiry; the pipeline
  restarts the candidate with the env, then restores), limits
  (32KiB headers -> 4xx/close, declared 100MB body -> 413/close, stalled
  half-request reclaimed <=12s with normal traffic flowing, all under a
  128MB cap via a pipeline restart).
- [x] J4. Mock validation: `/status` parsing + snapshot failures, cache
  gate pass + never-hits / no-Age / body-mismatch / never-expires fails,
  limits pass + careless-proxy fail, helper tests (35 proxy tests green).
- [x] J5a. Live smoke (2026-10-15, no model): a minimal forwarding proxy
  + the run-network nginx pool driven through the real pipeline with
  acceptance [1] — the proxy campaign path works end to end (task
  adapter, pool startup, acceptance-scoped verdict 1/1, stage-log
  contract); bench runs against the candidate.
- [x] J5b. Full-ladder LIVE proof (2026-10-15, no model): the reference
  proxy (`EDGE_MOCK_SERVER_PY` in `src/pipeline.rs`: round-robin,
  upstream timeout+retry, active health probes + `/status`, TTL cache,
  request limits) driven through the real pipeline with acceptance
  [1,2,3,4,6,7,8,9] — all 8 stages PASS
  (`test_pipeline_eval_only_proxy_full_ladder_live`, ~84s incl. the bench):
  docker-pause failover, quick failover under a hung upstream, the health
  surface flipping while idle, the cache restart (`SDB_PROXY_CACHE_TTL`),
  and the 128MB-capped limits stage.
  Harness bug found + fixed by this test: candidate restarts (stage 8/9,
  and the redis disk stages for consistency) did not re-attach to the run
  network (`connect_candidate_to_network`), so a restarted candidate could
  not resolve the upstream aliases (observed as an unbounded getaddrinfo
  stall against the host resolver). The initial start path already
  attached; restarts now do too.
- [ ] J5c. Model-driven grid cells for sdb-edge.
- [ ] J6. Later tickets: TLS termination with a harness-provided cert,
  and a throughput ticket (stage 5 + baseline gate) once proxy baselines
  are recorded.

## Slice K — sdb-queue (the new app: leases / ack / nack / retries / DLQ)

Third portfolio entry (DECISIONS #11 order) and the one genuinely NEW
application — no single-shot task existed, so the campaign is its primary
home. New task type `queue` (default port 6385), RULES `2026-10-15.3`.

- [x] K1. Task type + plumbing: `TaskType::Queue` (9 single-shot stages,
  port 6385), omp task guidance (protocol + anti-cheat), batch-ref None,
  CLI verify/replay arms, `tasks/queue/prompt.md`, and the config
  prompt-validation list.
- [x] K2. Verifier `src/verifier/queue.rs`: line protocol
  ENQ/LEASE/ACK/NACK/REDRIVE/STATS/PING with a contract header; stages
  1-9 (FIFO round trip; visibility + expiry + connection ownership; nack
  immediate/delayed + attempts; DLQ at 3 attempts with inspection-only
  reads; durability prepare/verify split for the pipeline kill; 6-consumer
  no-duplicate race; per-queue STATS; delayed ENQ + redrive; hardening
  with a 32KiB payload, malformed lines and slow-loris), plus a
  `measure_throughput` soak.
- [x] K3. Mock broker in the tests (~250 lines: leases, expiry, DLQ moves,
  redrive, per-queue counters, payload fidelity) with 8 injected defects
  (no-expiry, no-ownership, no-DLQ, duplicate delivery, frozen stats,
  void delays, oversized-ok, never-reclaim) each asserted to fail the
  intended stage with a naming error; plus a faithful-pass test over all
  self-contained stages.
- [x] K4. Pipeline arm: acceptance-scoped stages 1-9; stage 5 is the
  destructive durability half (prepare -> SIGKILL -> restart -> verify,
  skipped for tickets whose acceptance excludes it); stage 9 restarts the
  broker under a 128MB cap; setup/bench arms treat the task as
  verifier-timed (no reference container, jobs/sec soak).
- [x] K5. Campaign spec + prompts: nine tickets (genesis -> leases ->
  nack -> DLQ -> durability -> concurrency -> stats -> delays/redrive ->
  hardening) with cumulative acceptances and a dedicated base prompt.
- [x] K6a. LIVE proof (2026-10-15, no model): a stdlib-only reference
  broker (`QUEUE_MOCK_SERVER_PY` in `src/pipeline.rs`: connection-owned
  leases, nacks/delays, DLQ after 3 attempts with inspection reads,
  redrive, per-queue STATS, delayed enqueue, append-only fsync log under
  `./data` with torn-tail replay) driven through the real pipeline with
  acceptance [1..9] — all 9 stages PASS
  (`test_pipeline_eval_only_queue_full_ladder_live`, ~20s): FIFO round
  trip, lease expiry + ownership, nack immediate/delayed, DLQ, **the
  durability half (prepare -> SIGKILL -> restart -> verify: acked never
  return, in-flight returns with attempts preserved, DLQ intact)**, the
  6-consumer race (200 jobs, zero duplicates), STATS, delayed ENQ +
  redrive, and hardening under the 128MB cap + the jobs/sec soak.
  The live proof caught a real verifier bug: the durability prepare
  leased assuming FIFO would hand it the *prepared* job, but a correct
  FIFO broker returns the earlier *waiting* job — prepare now settles
  each state before enqueueing the next, and a mock test
  (`mock_queue_durability_halves`) covers both halves so it cannot
  regress.
- [x] K6b. First model cell (2026-10-15, ling-3.1-flash): **the full
  ladder — reached 8, 9/9 tickets, zero regressions, $0.2412** (see the
  sdb-queue grid log). More models next.

## Slice L — sdb-http (the last portfolio app: ranges / validators / compression / limits)

Fourth portfolio entry and the final app in the DECISIONS #11 order.
RULES `2026-10-15.4`. Task `http` (port 8080); the single-shot 5-stage
board (root/404/echo/user-agent/files) is unchanged.

- [x] L1. Verifier depth in `src/verifier/http.rs`: stage 6 byte ranges
  (206 + `Content-Range`, suffix/open, 416, plain-GET unchanged), stage 7
  conditional GET (`ETag` + `Last-Modified`, 304 semantics, stale
  validators, rewritten content changes the ETag), stage 8 gzip
  negotiation (decodable + smaller body via flate2; identity passthrough),
  stage 9 chunked uploads + `Expect: 100-continue` (no deadlock) +
  declared-128MiB refused at the head under a 128MB cap, stage 10
  hardening (three traversal spellings, 32KiB heads, malformed lines,
  slow-loris <=12s, liveness).
- [x] L2. Pipeline arm: acceptance-scoped stages 1-5 (single-shot path
  unchanged), campaign-only stages 6-10; 9/10 run after a 128MB-cap
  restart with the run-network re-attach; robustness probes stay advisory.
- [x] L3. Campaign: 8 tickets (t0 root+404 -> t1 echo/user-agent -> t2
  files -> t3 ranges -> t4 validators -> t5 gzip -> t6 chunked uploads +
  limits -> t7 hardening) with `http_stages` acceptance (1..=10) and
  prompts; validator range per task; spec loads and dry-runs.
- [x] L4. Mock rigor: a full mock file server (range slices, validators
  with real date comparisons, gzip via flate2, chunked/continue upload
  paths, traversal guard, head cap, stalled-socket reclaim) with **10
  injected defects**, each asserted caught by the intended stage with a
  naming error, plus a faithful-pass run over stages 6-10.
- [x] L5a. LIVE proof (2026-10-15, no model): a stdlib-only reference file
  server (`HTTP_MOCK_SERVER_PY` in `src/pipeline.rs`: keep-alive, echo/UA,
  `./data/files` store, ranges incl. suffix/open/416, ETag +
  Last-Modified with strict conditionals, gzip negotiation, chunked +
  `Expect: 100-continue` uploads, 16MiB declared cap, 8KiB head cap,
  traversal rejection, ~3s stalled-head reclaim) driven through the real
  pipeline with acceptance [1..10] — **all 10 stages PASS**
  (`test_pipeline_eval_only_http_full_ladder_live`, ~56s incl. the bench
  and the 128MB-capped restart that runs 9/10).
  Two findings from the proof: (1) the reference itself first missed the
  single-shot contract's `Content-Type: text/plain` on echo/user-agent —
  the verifier caught it (the fixture, not the harness); (2) the
  hardening stage only accepted 400/505 for a garbage request line, which
  would fail a correct 405/501 answer — tolerance widened to any 4xx or
  clean close (RULES `2026-10-15.5`, prompt wording updated).
- [ ] L5b. Model grid cells for sdb-http.

## Slice M — sdb-sql (the 21-ticket SQL ladder; spec: docs/PROPOSAL-sdb-sql.md)

Operator directive 2026-10-15: fix the review issues, document, build.
RULES for sdb-sql will be its own stamp when the first verifier lands
(sdb-sql is its own board, never pooled).

- [x] M1. Pre-build review fixes documented: 13 resolutions folded into
  the proposal (coverage gating only where the harness computes it, STATS
  semantics pinned, deterministic 5s-cancel query, oracle discard table +
  per-stage comparable-check floors + generator profile-coverage
  assertions, error-class mapping table, `reached` = highest stage passed,
  wall/disk guards, pinned `psql` client container, rollup/portal shape,
  rollback-aware prompts, full rollback scope, Q4/Q6 confirmed).
  DECISIONS #19 records the acceptance (supersedes #11 once landed).
- [x] M2. Repo-wide anti-cheat fix: references are detached from the run
  network for every verification window (`detach_references`/
  `reattach_references` around both `run_verification` call sites); the
  agent and the bench keep them. Live test pins resolve -> unreachable ->
  resolve. (This hole existed for ref-redis/ref-http in the live
  campaigns; sdb-sql inherits the fix.)
- [x] M3. Orchestrator foundations: `StageSpec` gains `tier`/`needs`/
  `lite {prompt, profile, budget_usd, max_turns, weight=0.4}`; task
  adapter accepts `sql` (`sql_stages`, range 1..=21); validation for
  tiers/needs/lite invariants; `LadderOutcome` + `stage_points` +
  `max_points` + `score_ladder` (tier x 1.0 full | weight lite | 0) with
  unit tests, and back-compat for the four existing specs.
- [x] M4. Orchestrator execution semantics (ladder modes): `LadderMode`
  (pass_stop for the archived app campaigns, continue for sdb-sql);
  `StageAttempt` (full|lite) carrying prompt/budget/turns/weight and a
  `rollback_first` flag; lite attempts after a failed full (rollback to the
  last green commit first, recorded as separate full/lite rows); `needs`
  skipping (blocked stages recorded `skipped`, zero cost); continue past
  failures; weighted points in the rollup (`points`/`max_points`,
  tier x 1.0 full | x weight lite); `reached` = highest stage passed (full
  or lite); optional campaign wall cap (`--wall-min`, `over_wall`). The
  CLI executor now performs the rollback (`git reset --hard` + `clean -fd`
  to the last green commit) and runs lite sessions with their own
  prompt/budget/turns. Unit tests: attempt order + rollback flags, lite
  scoring, needs-skipping, points/ceiling, back-compat for the four
  archived specs. Remaining M4 tail (moves with M6): the harness-owned
  data dir + snapshot/restore (sdb-sql's `--data` lives outside the
  workspace) and disk guards.
- [x] M5. Verifier core (`src/verifier/sql/`): **part 1 landed** — `mod.rs`
  (module + the self-test doctrine), `schema.rs` (seeded 3-5-table
  2-6-column schemas, deterministic per seed, dialect DDL emitter, one
  INT rowid-alias PK per table) and `oracle.rs` (in-process SQLite;
  DDL translation appending STRICT + TEXT-PK NOT NULL; error-class table
  with UNKNOWN messages loud, never guessed; discards for REAL/BLOB and
  both integer-overflow manifestations; file-backed twin for the
  oracle-vs-oracle gate). Nine tests including the oracle-vs-oracle corpus
  (memory vs file) and pinned dialect semantics (division/modulo signs,
  div-by-zero NULL, NULL ordering). The self-tests caught five real bugs
  before any candidate ran. **Part 2 landed** (5a-5c): `generator.rs`
  (seeded, type-directed; cumulative `Profile` per stage 0-3 + stage-9
  constraints; keyword-case/whitespace/`;` randomization; total-order
  `ORDER BY`; error probes incl. multi-row atomicity and the `a=b,b=a`
  swap; predicates anchored on inserted values; `build_script` runs each
  statement on a fresh oracle and fixes the expected answers up front),
  `compare.rs` (ordered vs multiset, headers, OK counts, error class,
  framing contradictions; cells compared in wire form), `wire.rs`
  (value escaping + the OK/ROWS/ERR response reader; no socket yet).
  Quality floors pinned in tests: >=90% gradeable statements, >=50% of
  filtered SELECTs return rows, >=60% of UPDATE/DELETE touch a row,
  <=3% constant WHEREs, every dialect error class probed. Dialect gaps the
  generator exposed are now in the spec (precedence verified against
  SQLite by brute force, backslash is literal, tab whitespace, no keyword
  identifiers).
  **Part 3 landed** (5d-5e): `run.rs` (`Candidate` trait + `run_script`;
  the seam the TCP client will implement), `shrink.rs` (ddmin over whole
  statements, every trial re-graded on a fresh oracle + fresh candidate;
  renders the model-facing bug report), and test-only `toy.rs`: a second,
  independent implementation of the stage 0-3 dialect written from the
  spec, with 26 planted bugs. Gates: the clean toy agrees with SQLite on
  every generated script (120 seeds x 4 profiles, plus a 3000-seed soak,
  ~900k statements, `--ignored`); every mutant is caught in >=25% of
  scripts (most in >90%); a 100k-statement memory-vs-file oracle gate; and
  shrink reduces real failures to <=5-6 statements. The toy found two
  generator soundness bugs the oracle could not see: SQLite silently
  promotes an overflowing intermediate to REAL (a comparison hides it), and
  dropped parentheses re-associated `5 * (x * NULL)` into an overflow.
  Fix: every expression and column carries a conservative magnitude/length
  bound (ints <= i64::MAX, text <= 48), parentheses are only dropped when
  the printed text parses to the generated tree, and explicit bare
  left-associative chains test associativity. Result: 0 overflow discards
  in 30k statements. **Part 4 landed**: `client.rs`, a TCP `Candidate`
  over one connection (timeouts, nodelay, one response per statement;
  split responses are reassembled and bytes past a response stay buffered
  for the next call). Review fixes on top of part 3: column bounds seeded
  from `DEFAULT`s, byte-safe DDL mask, loud oracle failure in
  `first_failure`, stage-0 error list corrected. **Part 5 landed**:
  `scenarios.rs` (harness-side pipelining client) and `driver.rs`
  (per-stage replay over the TCP candidate with shrink-on-failure and
  the health floors). **Profile lite subsets landed** with stage 5's
  `crash-acked-only` (the ladder narrows) and, in round 2, the
  per-ladder `passed_lite` propagation that keeps a stage's variant
  graded in every later ticket. M5 complete.
- [x] M6. Reference adapter (sqlite3-backed, text protocol) + live proof
  for stages 0-3 through the pipeline, then one cheap-model cell.
  **Adapter + live proof landed**: `ref_server.rs` is a stdlib-only
  Python server backed by its own sqlite3 (STRICT DDL translation, error
  classes, total-changes affected counts); it passes every stage 20/20
  at the verifier level and stages 0-3 through the real pipeline
  (eval-only, one container, four scripts against one long-lived server
  — schemas are seed-namespaced so stage scripts cannot collide).
  Pipeline: `TaskType::Sql` (port 5480), the `run_verification` branch
  (spawn_blocking, shrink repro on failure, verifier faults as infra
  errors), `eval`/`replay` support, omp guidance. `campaigns/sdb-sql`
  ships tickets 0-3 with the cumulative acceptance; validation accepts
  the 0-based ladder. **ref-sql landed**: `SandboxManager::start_reference_sql`
  mounts the generated Python adapter (`ref_server.rs`) into the reference
  image on the run network as `ref-sql`, detached during verification like
  every other reference; a live test probes it through the alias. **Cell
  tooling**: `scripts/sql-cell.sh` rebuilds the release binary, refuses a
  stale one (marker check; cell 2 ran the old verifier for exactly this
  reason), preflights the engine and port 5480, then runs the campaign.
  **Cells (2026-10-16, `inkling-small:free`, cap $0.25/40min)**:
  - cell 1 (ticket 0, pre-fix): FAIL 0/1, $0.0196. It exposed two verifier
    bugs: `shrink` replayed trials against the live candidate, so a
    stateful server answered `exists` at the first CREATE and the
    minimizer reported that as the bug (the original failure was a 10s
    read timeout); and the repair round reused the run seed, so a
    persisting candidate re-created its own tables and failed before
    anything was graded. Fixed (commit "M6 fixes"): the driver takes a
    reset callback (the pipeline wipes `<workspace>/data` and restarts
    the container), shrinks only after a full replay on the reset state
    reproduces the failure, and falls back to the original run's prefix
    otherwise; the repair round uses a fresh seed.
  - cell 2 (ticket 0 -> ticket 1, pre-fix binary): **reached ticket 0** —
    s0 PASS 1/1 ($0.0089, 25 turns), s1 FAIL 0/2 ($0.0110, 30 turns),
    total $0.0199. Ran the *pre-fix* release (not rebuilt), so it
    validates the pipeline and campaign wiring, not the fixes: ticket 1
    failed on genuine candidate bugs (CREATE reporting the column count
    as affected, INSERT into a missing table answering `OK 2`, then a
    parser/semantics error), and the repro drift it shows is the old
    shrink bug.
  - cell 5 (2026-10-16, tickets 0-4 ladder, after ticket 4 landed): FAIL
    ticket 0 — the candidate answered `ERR arity` where the reference
    answers `ERR syntax` for the truncated-statement probe
    (`INSERT INTO t VALUES (1,`). Genuine candidate bug; the report is a
    1-statement minimal repro after 10 shrink trials, i.e. the
    throwaway-store shrink and the readiness probe work in a live cell.
    $0.0134, 40 turns, reached none. (A first attempt aborted in the cell
    script's stale-binary guard — a stale *marker*, fixed; no spend.)
  - cell 3 (fix validation, rebuilt release): **reached ticket 0** — s0
    PASS 1/1 ($0.0099, 28 turns), s1 FAIL 0/2 ($0.0086, 30 turns), total
    $0.0185. The candidate persists its store in `./data`, so both fixes
    were exercised for real: the repair round ran under a fresh seed (two
    distinct table prefixes in the log, no `exists`), and the failure
    report is a truthful 6-statement minimal prefix whose statement and
    detail match the stage failure (`INSERT ...` -> `ERR syntax`, expected
    `OK 1`). Known limitation: when the coarse mismatch kind allows it, a
    minimized repro can name a different statement than the stage's first
    failure; it is always a real failure on a fresh database, and fixing
    it re-runs the suite.
  **Review fixes (2026-10-16, after merging the branch)**: an unreachable
  server is a failed stage, not a verifier fault (it was scored "fault":
  no lite try, "measured nothing"); `check_no_embedded_sql` scans every
  file (no test-name exemption) for SQLite/DuckDB/parser imports,
  literals, Go paths, crates, binaries, vendored sources, `SQLite format
  3` magic in data files and binaries, sql task only; the stage-0
  scenarios the prompt promised now run (pipelined script answered in
  order, 8 concurrent clients see each other's rows, garbage bytes /
  unterminated line / 2 MiB line must not kill the server); the fresh
  repair seed is sql-only (other tasks keep their grading); `SPEC.md`
  (every earlier ticket + the current one) is written before each sql
  session; prompts and omp guidance name `ref-sql:5480`; acceptance ids
  are 0..=20; `stage_seed` is FNV-1a (stable across toolchains); the
  reference's DDL match tolerates randomized whitespace. Tickets 0-3 have
  no lite by the spec, so none is missing.
  - cell 4 (fixed build, 2026-10-16, `inkling-small:free`): **reached
    ticket 0** — s0 PASS 1/1 ($0.0101, 28 turns; the new scanner and the
    stage-0 scenarios included), s1 FAIL 0/2 ($0.0142, 30 turns) as a
    stage-0 regression, total $0.0242. The first attempt aborted before
    any model call (SPEC.md written before the genesis workspace existed;
    fixed by handing it to the pipeline). Verified by hand: ticket 0's
    commit answers the 4-statement repro `OK 4`, ticket 1's `ERR syntax`,
    so the regression is real. Ticket 1's rewrite also dropped `--port`
    (always 5480); the harness never passes it, so that contract clause
    is unchecked.
- [~] M7. Tiers 2-7 in order (storage/crash/transactions -> relational ->
  engine -> ops -> resilience -> distributed), each with a live proof and
  the 20/20 flake bar.
  **M7b landed (2026-10-16)**: the sdb-sql launch contract is real —
  `CandidateRunExtras.launch_args` appends `--port <p> --data /sdb-data`
  to the candidate's argv (start.sh sandbox path and custom Dockerfile
  both), and the pipeline binds a harness-owned data directory
  (`<campaign dir>/sql-data`, a sibling of the workspace so it survives
  restarts and stage rollbacks) to `/sdb-data`; the reference's
  `start.sh` honors `"$@"` with baked defaults. A live sandbox test
  proves the argv and the writable mount reach the container, and the sql
  eval-only live proof runs on the new path.
  **Ticket 4 landed (2026-10-16)**: `sentinel.rs` (seeded datasets,
  ordered reads recorded from the oracle; written when a stage >= 4
  passes, re-checked reads-only before later suites; `drift:` is an
  upgrade regression that fails the writing stage again), the
  `scenarios::persistence` restart scenario wired through a new
  `restart` callback on `run_stage`, the store snapshotted/restored with
  the workspace on rollback, the campaign ticket (tier 2, acceptance
  [0..4]) and its prompt, and the reference no longer wiping its
  database at startup. Gates: 20/20 at every stage 0-4 against the
  reference (real restarts), a stage-4 live proof through the pipeline
  (5/5 + sentinel written), and a drift proof (stages 0-3 green, stage 4
  red with `[upgrade]`).
  **Ticket 5 landed (2026-10-16)**: `crash.rs` holds the crash ladder's
  protocol and invariants (acked rows exactly, a 50k-row UPDATE
  all-or-nothing, a torn partial trailing record, double restarts) with
  kill/restart/truncate as callbacks; the pipeline owns the ladder
  (`crash_ladder`) and fails stage 5 through the same late-failure path
  as upgrade drift. The reference runs WAL + synchronous=FULL so acked
  commits survive SIGKILL and a torn WAL tail recovers; the reference
  crash test SIGKILLs and restarts a real process. Gates: stage-5 live
  proof under Docker (6/6, all four [CRASH] lines), sentinel for stage 5
  written, `differential_supported` reaches 5.
  **Stage-5 lite landed (2026-10-16)**: `LiteSpec.profile` now reaches the
  verifier (`StageAttempt.lite_profile` -> `StageJob` ->
  `CampaignStageConfig.lite_profile` -> `run_verification`, opaque flags);
  the sql arm maps `crash-acked-only` to the narrowed crash ladder (acked
  rows + double restart; no bulk-update or torn-tail checks), and the
  campaign declares `stages/5_crash_lite/prompt.md` (weight 0.4, 60% of
  the stage's budget/turns). Gate: a lite live proof (6/6, only the
  acked/restart [CRASH] lines run). **Remaining**: the proposal's
  cumulative-variant rule — after a lite pass, later tickets' suites must
  keep the lite profile for that stage, which needs the rollup's
  per-stage variants passed into the pipeline (like `accepted`); a cell
  for tickets 0-5 is also outstanding.
  **Review fixes landed (REVIEW.md, 2026-10-16)**: the http health probes
  are bounded again; the sql shrink runs on throwaway stores (the real
  `sql-data` survives for ticket 4) and the driver waits for a
  protocol-level answer before replaying — docker's port proxy accepts
  before the server inside serves, which had made every reset untrusted;
  the embedded-SQL import check covers comma/`;`/`:` forms; Dockerfile
  candidates get `--port`/`--data` only with an `ENTRYPOINT` and read
  `SDB_PORT`/`SDB_DATA_DIR` otherwise; RULES 2026-10-16.7.
  **Ticket 6 landed (2026-10-16)**: the generator has a `txn` profile —
  whole transactions (`BEGIN`, a DML body, `COMMIT`/`ROLLBACK`), nested
  `BEGIN` and bare `COMMIT`/`ROLLBACK` probes, `Kind::Txn`, and scripts
  always closed (`build_script` drains an open transaction). The toy
  engine implements transactions, so the independent-agreement gate now
  covers the stage-6 scripts too. `txn.rs` holds the scripted checks the
  generator cannot express: cross-connection isolation (uncommitted rows
  invisible, `COMMIT` visible, `ROLLBACK` gone) and a kill with a
  transaction open (committed survives, uncommitted leaves no trace),
  wired through the pipeline's late-failure path for stage 6. The
  reference passes both against a real process; the campaign declares
  ticket 6 (tier 2, acceptance [0..6], no lite per the proposal). Gates:
  20/20 at every stage 0-6, a stage-6 live proof (7/7, both `[TXN]`
  lines, sentinel written).
  **Round-2 review fixes landed (REVIEW.md, 2026-10-16)**: restarts that
  model a clean stop are SIGTERM (`stop_candidate_gracefully`, used by
  ticket 4's restart and the post-sentinel flush) while ladder/shrink
  restarts stay SIGKILL; the crash kills now land mid-statement (send-only
  + a probe that says whether the never-read statement answered, and when
  it did the row is owed exactly); the bulk UPDATE retries with a bigger
  table until the kill lands in flight; (c)'s torn tail kills first, tears
  the *log* file the victim commit touched (never the database; SQLite's
  `-shm` skipped, WAL preferred by name), retries when a checkpoint
  rewrote in place, and the reference keeps a long-lived connection so its
  WAL survives between harness connections; the ladder runs on a scratch
  copy of the store (the persistent one stays small); stage 6's isolation
  probe tolerates a global-lock candidate (bounded async read, released by
  COMMIT/ROLLBACK, busy retried); each stage's lite variant is remembered
  per ladder (`passed_lite`) so ticket 6+ keeps grading stage 5 lite; late
  failures carry `[crash]`/`[txn]`/`[upgrade]` classes; a sentinel
  transport error *after* the store answered is drift, not "not
  checkable"; the campaign snapshot/copy never follows symlinks or FIFOs
  and includes the sentinel chain; in-transaction scripts now mix SELECTs
  and statement-level errors; the toy models SQLite 3.46's statement
  journal (a type error keeps earlier `VALUES` rows inside a transaction,
  rolls back in autocommit) and the reference emulates the autocommit
  case on the host's 3.45; Docker launches append `--port`/`--data` to the
  image's *CMD* (inherited entrypoints keep their command); shrink stores
  are per-instance and cleaned after the stage; plus a 20x bar for the
  ladder and txn checks.
  **Ticket 7 landed (2026-10-16)**: the generator has an `aggregates`
  profile — `COUNT(*)`/`COUNT(x)`/`SUM`/`MIN`/`MAX`, scalar aggregates
  (one row even over no matching rows), grouped queries whose key text is
  repeated in SELECT/GROUP BY/ORDER BY (a total order, so the frame
  compares as a sequence), and `HAVING` over aggregates and the key.
  Fairness rules: the select list never mixes an aggregate with a bare
  non-grouped column, `SUM` is INTEGER-only and generated only while
  `bound x row bound` fits in 64 bits (a REAL promotion is ungradeable),
  `ABS`/arithmetic aggregate arguments are type-checked, and the
  multi-row statement-atomicity probe is now autocommit-only (inside a
  transaction SQLite 3.46 keeps the rows an earlier `VALUES` inserted,
  which the dialect leaves untested). New aggregate error probes
  (`GROUP BY zz_nocol`, `SUM(zz_gone)`, an aggregate in `WHERE`, an
  aggregate as the `GROUP BY` key) and their classes pinned in the oracle
  and the reference (`syntax`). The toy implements aggregates, `GROUP BY`
  (NULL is one group) and `HAVING`, gained two mutants (`COUNT` counts
  NULLs, per-NULL groups), and the independent-agreement gate now covers
  stage 7 (120 seeds x 8 profiles) plus the 3000-seed soak at 300 steps.
  `differential_supported` reaches 7; the campaign declares ticket 7
  (tier 3, needs 1, acceptance [0..7]); RULES 2026-10-16.8. Gates: the
  20/20 bar extended through stage 7, a stage-7 live proof under Docker
  (8/8, sentinel written), and the aggregate spec text updated (`SUM`
  INTEGER-only, the empty-group contract, the aggregate misuses).
  **Ticket 8 landed (2026-10-16)**: the generator has a `joins` profile
  (comma lists and `INNER JOIN`, plus `LEFT [OUTER] JOIN` in the full
  variant) over 2-3 `FROM` items — distinct tables or a self-join with
  aliases — with `ON` link predicates over type-compatible columns,
  qualified references (`a.x`), bare references when the name is unique
  across the join, `SELECT *` in FROM order ordered by every column, and a
  total-order requirement on every emitted frame. New probes: an ambiguous
  bare name in a self-join (`ERR ambiguous`), an unknown qualifier, a
  missing `ON` column, and an unknown joined table. The toy resolves
  per-statement names (bare -> unique or `ERR ambiguous`, qualifier ->
  item, source-column `ORDER BY`), executes nested-loop joins with
  NULL-padded `LEFT JOIN` rows, and classifies `ON` columns at prepare
  time; it gained an `LeftJoinIsInner` mutant. The agreement gate covers
  stage 8 (120 seeds x 9 profiles) and the 3000-seed soak runs stage-8
  scripts; the stage-8 live proof (9/9) and its lite variant both pass
  under Docker. Driver: `run_stage_with_variants` narrows the cumulative
  profile per the variant each stage passed at (the pipeline passes the
  current attempt's `lite_profile` plus the ladder's `passed_lite`), so
  `join-inner-only` drops `LEFT JOIN` in the stage-8 lite suite and in
  every later ticket. The campaign declares ticket 8 (tier 3, needs 1,
  acceptance [0..8], lite `stages/8_joins_lite/prompt.md` weight 0.4);
  RULES 2026-10-16.9; the 20/20 bar runs through stage 8 (180 stage
  runs).
  **Ticket 9 landed (2026-10-16)**: the schema's constrained DDL now
  includes `REFERENCES` (a later table's child column pointing at the
  first table's INTEGER primary key) beside PRIMARY KEY/UNIQUE/NOT
  NULL/DEFAULT, and the generator emits it only from stage 9; foreign-key
  cells read the parent's inserted values (or NULL — an FK ignores
  NULLs), and probes cover a NULL into `NOT NULL`, a duplicate `UNIQUE`
  value and a child with no parent. The toy enforces the whole contract:
  INTEGER primary-key auto-assignment (max+1, 1 when empty), TEXT keys
  NOT NULL, defaults for omitted columns (an explicit NULL does not get
  the default), UNIQUE with several NULLs allowed, and restrict-semantics
  foreign keys on insert, update and delete — with the FK pass at the
  *statement end* because SQLite's immediate constraints fire there
  (a later row's type error wins over an earlier row's missing parent,
  which the soak found). The toy gained a `ConstraintIgnored` mutant; the
  agreement gate covers stage 9 (120 seeds x 10 profiles) and the
  3000-seed soak runs stage-9 scripts. A generator bounds bug the soak
  found (an auto-assigned key can exceed the column's recorded bound) is
  fixed. Gates: 20/20 through stage 9 (200 stage runs), a stage-9 live
  proof (10/10), RULES 2026-10-16.10.
  **Round-3 review fixes landed (REVIEW.md, 2026-10-16)**: the acceptance
  list is now the *passed* stages plus the current ticket (a failed or
  skipped stage contributes nothing; before, one storage failure sank
  every later ticket), and the campaign/ladder carry the passed set
  (`StageAttempt.passed`); the crash probes keep the socket open *through*
  the kill and read it afterwards, so "the statement completed first" is
  measured after the kill (an answered statement is owed: `check_uniform`
  requires the new value then), and the (b) retry doubles to 400k rows;
  a clean stop delivers SIGTERM to every process in the container
  (`docker exec ... kill -s TERM -- -1`, since `docker stop` reaches PID 1
  and a non-`exec` start.sh would swallow it); statement atomicity is
  graded identically in autocommit and inside a transaction (the oracle
  wraps every statement in a savepoint and the reference does the same, so
  no SQLite statement-journal version quirk leaks into grading); aggregate
  and join queries gate `ORDER BY`/`LIMIT` on ticket 2 (tickets 7/8 need
  only 1, and without ticket 2 their frames compare as multisets); after a
  torn tail every earlier acked victim is owed (`check_keys`: only the
  newest may vanish); only `ERR busy` from the isolation peer is
  retryable; the crash ladder always restores the real store and drops the
  scratch copy; test temp dirs are guard-cleaned (TestDir/RefProc) and
  stale reference staging dirs are swept; RULES 2026-10-16.11. **Next**:
  - cell 6 (2026-10-16, `inkling-small:free`, after the round-3 fixes):
    s0 PASS 1/1 ($0.0103, 28 turns), s1 FAIL 0/2 ($0.0110, regression on
    stage 0) — and the run **stopped after s1**: the shipped spec never
    declared `ladder`, so it defaulted to `pass_stop` and no cell had ever
    exercised "the ladder continues" (fixed in `081d513`, pinned by
    `shipped_sdb_sql_campaign_continues`).
  - cell 7 (2026-10-16, rerun with `ladder: continue`, $0.0504):
    s0 PASS 1/1 ($0.0267, 26 turns); s1 FAIL 0/2 ($0.0092, regression on
    stage 0); s2/s3 skipped (`needs` 1); **s4 ran with acceptance [0,4]**
    — exactly two stages in the suite, so the passed-set intersection
    (finding 1 of round 3) works end to end — s4 FAIL 0/2 ($0.0145, stage
    0 broken again by the candidate's rewrite, so stage 4 was skipped
    within the verification); s5-s9 skipped by `needs`; reached ticket 0,
    $0.0504 total. The candidate regresses stage 0 on every rewrite
    (genuine: `expected ERR exists, got ERR syntax`), which is the ticket
    it should repair before climbing.
  - cell 8 (2026-10-17, `ling-3.1-flash`, cap $0.75 / 120 min, round-4's
    stronger model): **reached ticket 5** — the deepest run so far and the
    first real end-to-end drive of tickets 4-8. s0 PASS 1/1, s1 PASS 2/2,
    s2 FAIL 0/3 (regressions 0,1), s3 PASS 3/3, s4 PASS 4/4, s5 FAIL 0/5
    full then **PASS 5/5 on the lite crash variant**, s6 FAIL 0/6, s7
    FAIL 5/6 (aggregates almost), s8 FAIL 2/6 full + 0/6 lite. 5
    regression-free tickets, $0.7722, tickets/$ 6.5, `over_budget` (the
    final repair round crossed the cap; recorded, never a model verdict).
    Notable: the full s8 attempt passed 5/6 (only Joins red) and lost it
    in its *repair* round — the agent's last edit left `elif c == "*":`
    without a body (`IndentationError`), the server never started and
    every stage failed; the lite attempt repeated the pattern (5/6
    initial, 0/6 repair). The s8 shrink repro printed `expected
    no_table` where the stage failure said `no_column`: the 1-statement
    repro runs on a fresh database where the FROM table no longer exists
    — the documented coarse-kind limitation, not a verifier bug.
    Harness gap found and fixed: the served board still showed the
    mid-run snapshot ($0.4967) because nothing rebuilt it when the
    campaign ended; the runner now refreshes the sdb-sql board when it
    writes the final rollup.
  **Ticket 10, part 1 landed (2026-10-16)**: the `subqueries` profile and
  the toy's subquery engine — scalar subqueries in select items and
  comparisons (always aggregate-wrapped, result-type matched so no
  mixed-type comparison creeps in), `[NOT] IN (subquery)`, `IN (list)`/
  `NOT IN` (three-valued: a NULL in the list makes a non-match NULL, and
  an *empty* set is false even for a NULL left side), `[NOT] EXISTS` with
  a correlated predicate, correlated references via a scope chain
  (`Ctx::outer`), static validation (a subquery's columns and its
  one-column arity are checked before any row exists), `INSERT ... SELECT`
  execution (shared per-row checks with `VALUES`), the four subquery error
  probes, and an `InNullIgnored` mutant. The independent agreement gate
  now covers stage 10 for those shapes (120 seeds x 11 profiles) and the
  mutant suite runs over stage 10. **Pending for the completion commit**:
  (a) the generated `INSERT ... SELECT` form stays at weight 0 — the gate
  caught a divergence around SQLite's rowid assignment for a mixed
  NULL/explicit multi-row insert into a constrained table (seed 41,
  statement 73: the oracle takes 3 rows, the toy errors), which needs one
  more debugging round before the shape can be graded; (b) the ticket
  declaration + prompt, `differential_supported(10)`, the 20/20 bar
  through 10, a stage-10 live proof and the RULES bump.
  **Ticket 10 completed (2026-10-16)**: the remaining divergence was a
  *generator* issue, not the toy's: SQLite's INTEGER PRIMARY KEY
  auto-assignment uses "largest rowid so far + 1" and never looks ahead at
  the statement's later explicit values, so a *mixed* multi-row insert
  (some NULL keys, some explicit) makes the outcome depend on the key
  assignment order. The generator now picks one mode per statement (all
  explicit or all NULL) and `INSERT ... SELECT` copies the source column's
  value bounds to the target (a SUM over the target could overflow past
  its stale bound) and never yields a NULL key. The toy also short-circuits
  `LIMIT 0` before evaluating aggregates, as SQLite does. With those
  three fixes the agreement gate is green over stage 10 including
  `INSERT ... SELECT` (120 seeds x 11 profiles). Gates: `differential_supported`
  reaches 10, the campaign declares ticket 10 (tier 3, needs 7, no lite),
  the 20/20 bar runs through stage 10 (220 stage runs), a stage-10 live
  proof passes (11/11), RULES 2026-10-16.12. **Next**: ticket 11 (indexes
  + perf gate): the D side needs `CREATE [UNIQUE] INDEX`/`DROP INDEX` in
  the generator and the toy (unique enforcement on insert/update,
  duplicate-index-name and missing-column errors, index survival in the
  reference), and the S side is a perf gate (200k rows, 500 point lookups
  on an indexed column vs an unindexed copy: p50 < 5 ms and >= 10x
  faster, ranges only in the full variant), with the lite profile
  `index-equality-only`.
  **Ticket 11 landed (2026-10-17)**: see the ticket-11 record below — the
  generator's index D side plus round-4 finding 3 (claimed there, but the
  real `pick_other_table` port landed only with round-6's `.15`),
  the toy's independent index engine, the perf gate (`perf.rs`, under the
  bench lock, restart included), `differential_supported(11)`, the
  campaign ticket (tier 4, needs [3, 4], lite `index-equality-only`) and
  both prompts. Gates: the stage-11 soak is 0/3000, the agreement gate and
  the mutant sweep run through profile 11, and the 20/20 bar now covers
  12 stages x 20 runs (240 stage runs); two Docker live proofs pass
  (full 91 s, lite 87 s, both 12/12 with `[PERF]` lines). RULES
  `2026-10-17.14`. Tier 2 is now fully implemented on the harness side
  (tickets 3-4 landed, 5-6 landed, 7-11 landed).
  **Round-4 review (REVIEW.md, 2026-10-17)**: finding 1 (HIGH) fixed — the
  graceful stop signalled every process *except* PID 1 (`kill(-1)` skips
  init), so an `exec`'d server (the recommended form) got one second from
  `docker stop -t 1` after a wasted 5 s wait; now `docker kill -s TERM`
  reaches PID 1 with no shell in the image, the other processes get
  `kill -TERM -1` (the spelling dash and busybox both accept), and the
  whole timeout is theirs to flush. Pinned by an exec-form PID-1 fixture
  whose handler sleeps 2 s (the old code lost the marker) plus the
  existing non-exec one. Finding 2 (MEDIUM) partly fixed: every (b)
  attempt's table is now checked right after its own restart (an answered
  update is owed), and (c) requires the bulk value (b) read — a torn tail
  may cost the newest victim, never an update committed before it.
  **Still open from round 4 (ride with ticket 11)**: (2, rest) (d) must
  also require the key set and bulk value it saw before; (3) `EXISTS`/`IN`
  subqueries whose inner table is the outer one are effectively
  uncorrelated (re-pick the inner table, and optionally add a same-table
  aliased corrlation); (4) `is_busy` string-matches a `Debug` rendering
  (make the peer error typed); (5) NITs (`CampaignStageConfig.
  passed_stages` is written but never read; log the `docker exec` stderr
  once — the latter is done in the finding-1 rewrite). REVIEW.md also
  asks for a cell with a stronger model before ticket 11, so tickets 4-10
  get a real end-to-end run. **Round-4 fixes completed (2026-10-17)**:
  (2, rest) every (b) attempt's table is checked after its own restart and
  (c) and (d) pin the bulk value and the *whole key set* (d saw before its
  restart) — a second restart that changes anything fails; (a)'s in-flight
  row upgrades from `maybe` to `must` once it is visible after the first
  restart; (4) the isolation peer read now answers a typed
  `PeerErr::{Busy, Other}` and only the `busy` *class* is retryable (no
  string matching of a `Debug` rendering). **Still open**: (3) EXISTS/IN
  subqueries whose inner table is the outer one are uncorrelated (re-pick
  the inner table + the outer-column test) — rides with ticket 11; (5)
  `CampaignStageConfig.passed_stages` (the stderr logging half is done).
  The stronger-model cell before ticket 11 is still outstanding.
  **M7 plan**: 4 persistence (restart keeps data + upgrade check),
  5 crash safety (SIGKILL after acks, mid-statement kill, torn tail,
  double restart — reuse the sdb-kv machinery), 6 transactions
  (BEGIN/COMMIT/ROLLBACK profile + txn probes + isolation/kill
  scenarios), then tier 3+ in ladder order.
  Oracle gaps found by a 2026-10-16 probe of later-stage SQL (fix when
  the generator starts emitting them): unmapped messages "no such index",
  "duplicate column name", "there is already another table or index with
  this name", "aggregate functions are not allowed in the GROUP BY
  clause", "has more than one primary key"; `SUM(text)` discards as REAL.
  (ALTER on a STRICT table answering `ROWS` instead of `OK 0` is fixed.)
- [ ] M8. Retirement switch for the old tasks/campaigns, portal labels,
  README rewrite; calibration run; grid on the new stamp.
  **Public surface is sdb-sql-only (2026-10-17)**: the console's task
  pickers, leaderboard/matrix task lists, defaults and hints offer only
  `sql`; `/api/campaigns` lists only the active campaign
  (`campaign::ACTIVE_CAMPAIGNS = ["sdb-sql"]`); the served portal is a
  new sdb-sql campaign board (`PortalExporter::export_sql_board`, ranked
  by difficulty-weighted points, then points per $, with reached and
  regression-free shown; round-6 finding 4) and the stale
  root-owned `portal/runs/` tree of legacy per-run pages is gone (the
  silent `remove_dir_all` failure now logs). `tasks/sql/prompt.md` feeds
  the spec editor. Retired tasks remain runnable from the CLI — their
  verifiers and campaigns are untouched, so the full code-level removal
  (a destructive sweep) and the README rewrite are the remaining M8
  items, with the calibration run and the grid.

**Ticket 11 (indexes + perf gate) — landed (2026-10-17)**: the reverted
increment was re-applied by hand on top of the round-5 fixes (the stash
would have conflicted with them): the generator's `indexes` profile
(stage 11) with `CREATE [UNIQUE] INDEX` / `DROP INDEX` interleaved with
DML and three probes (missing table, missing column, missing index on
`DROP`), the oracle/reference `no such index` -> `no_table` mapping,
round-4 finding 3 — only *claimed* here: the code still called
`pick_table` in the three subquery arms until round-6 finding 1 landed
the real port with `.15` (`pick_other_table`: a subquery's inner table is
never the outer statement's — an overlap would bind every correlated
name to the inner scope) — the toy's index engine (parser + registry + UNIQUE
enforcement on INSERT/UPDATE + a transactional snapshot + the
`exists`/`no_table`/`no_column` classes), the perf gate (`perf.rs`: two
200k-row tables, 500 alternating point lookups, p50 < 5 ms and >= 10x,
range windows in the full variant only, and a restart between two rounds
so a memory-only index fails), `differential_supported(11)` via
`MAX_DIFFERENTIAL_STAGE = 11`, the campaign entry (tier 4, needs [3, 4],
lite `index-equality-only`, $0.15 / 36 turns) and both prompts. Gates:
the **stage-11 soak is 0/3000**, the agreement gate and the mutant sweep
run through profile 11, 111 verifier tests, the **20/20 bar through
stage 11**, the reference perf-gate test (full + lite + restart), two
Docker live proofs (full 91s and lite 87s, both 12/12 with `[PERF]`
lines), fmt and clippy clean; RULES `2026-10-17.14`. **Still open**: the
stronger-model cell the round-4 review asks for (it was requested
*before* the perf gate, which now exists); the round-4 finding-2
fixture remainder (b)/(c)/(d) and finding-5 (`passed_stages` written but
never read) still ride with ticket 12.

**Round-7 review fixes landed (2026-10-17)**: (1, HIGH) the campaign
launcher is now safe: `campaign run` itself takes a **campaign lock**
(`runs/.campaign.lock`, `{pid, campaign_id}`, stale locks from dead
holders replaced), so the CLI and the GUI cannot overlap; `POST
/api/campaigns` is proxied like every other mutation, never touches
`is_running`, refuses while the lock or a wave is live (409), validates
the effort against the known values and caps the budget at $5 unless
`confirm: true`; the engine's queue pump holds SQL jobs while the lock
is live; the child is reaped with its exit status appended to
`runs/campaign-launch-<ts>.log`, whose path comes back in the response;
and `/api/status` reports the lock holder, so the 409 and the status
agree. (2, MEDIUM) the perf gate compares **net work** over the
round-trip floor (indexed work <= scan work / 10 or <= 0.05 ms) instead
of allowing 2x the floor, so a constant per-statement overhead can no
longer hide a missing index; a fake-engine test pins the sleep case and
RULES moves to `2026-10-17.16`. (3) the regression cause no longer
blames stages the verifier skipped: `broke ticket 0 (1,3,4,5 not run):
…`. (4) per-round `$` renders as `≈$` with a "token estimate" tooltip
(the attempt's effective cost stays the bill). (5) both cap flags are
set when both were crossed at the same boundary, and the Run header
says caps are checked between tickets. (6) a `running` cell whose
writer is gone (no live lock, no update for 30 minutes) shows
`stalled`, never "running" forever. Tests: campaign lock
(refuse/replace/release), `effective_state`, the classifier's skipped
tail, the proxied campaign POST, the merged validation+lock+status
test; full lib suite 876 green; the CLI refusal and the API 409 were
verified live.

**Round-12 review fixes landed (2026-10-18, portal + ops; rules still
`2026-10-18.17`)**. P1 (HIGH, the board kept getting wiped): pipeline step 13
is deleted — the retired legacy exporter no longer runs after saved runs — and
every writer now goes through `RunArchiver::resolve_portal_dir()`
(`SDB_PORTAL_DIR`-aware, set by `isolate_test_runs_dir`), so `cargo test`
cannot overwrite the served `portal/` (proved by hashing `portal/index.html`
across a full suite run). The CLI `portal` command and `regen-reviews` now
publish `export_sql_board` too; no code path can write the legacy page.
P2: `export_sql_board` derives `campaign::effective_state` (muse shows
`stalled`, not RUNNING). P3: the portal board matches the console — labelled
ladder squares + one legend, repeat runs collapsed into `<details>×N runs`
(folded runs named by campaign id, median noted), the "older rule sets
(incomparable)" divider. Note A is folded into the `.17` rules history: the
0.25 API-ratio default makes a $0.75 campaign's ticket 0 start at $0.1875 —
the cap's effective ceiling binds, by design. Note B: after every rebuild,
`sudo systemctl restart subdollarbench-engine subdollarbench-gui` (done; the
units serve the current binary). **The DeepSeek `.17` re-run (10:37-10:59) is
INVALID and must not be read as a model result**: the round-10 harness note
started with `---`, and `omp -p <note>` parsed it as an unknown flag, so
every attempt after ticket 2's failure crashed at round 0 (0 turns) and the
verifier graded the previous ticket's code (round-13 R1). Ticket 2 is the
only genuine model failure in that cell. The cell is poisoned ("R1: omp argv
crash, invalid") and is not graded; re-run after R1.

**Round-13 review fixes landed (2026-10-18, rules still `2026-10-18.17`)**:
R1 (CRITICAL) — both harness builders end their argv with `--` and then the
positional prompt (`sandbox::push_prompt_args`; `omp_task_args` returns the
pure tail), the harness note opens with a plain line ("Harness note (added by
the benchmark, not the ticket):") and closes with "End of harness note.", so
a `-`-leading prompt can never be read as a flag again; a live proof drives
the real `omp` and `opencode` parsers in the sandbox image in both directions
(`live_test_harness_cli_parses_a_dash_leading_prompt_after_separator`), and
the source guard test forbids `.arg(&effective_prompt)` coming back. An agent
that exits before its first turn is now a **harness fault**: the round is
retried once, and if it still never ran, the round is not graded at all
(`[FAULT] verification skipped`, outcome `infra_error`), so a 0-turn crash
can never again be recorded as a stage result. B1/B2: the portal groups runs
by (model, harness, effort, rules) — incomparable rule sets never share a row
— and the divider uses the shared `campaign::is_current_rules` predicate
(unknown rules count as not current); hidden and poisoned *cells* now publish
nowhere (portal + console), with tests for each. B3: the legend's swatches
have real CSS colours and list all seven statuses. Ops for opencode cells
(2026-10-18): the harness works, but two host preconditions — (1) the API key
must come from `.env`; a stale `OPENROUTER_API_KEY` exported in the calling
shell made opencode 401 at round 0 and burned a fault cell
(`scripts/sql-cell.sh` now prefers `.env` and says so); (2) the model must be
in opencode's catalog — `deepseek/deepseek-v4.1-flash-1017` was not
(models.dev lags OpenRouter), giving `provider.no-route: Model unavailable`;
try `opencode models` inside the sandbox and add an entry under
`provider.openrouter.models` in the host `~/.config/opencode/opencode.json`
(the sandbox mounts it). A provider auth/billing event on stdout now names
itself (`[BILLING] opencode provider.auth: … (check the API key …)`) instead
of "exited with no further detail".

**First cell on the fixed argv (2026-10-18 12:27-13:24, opencode · effort
max · $0.75/120 · rules .17)**: **reached ticket 5, 6 regression-free,
$0.6077, 9.9 tickets/$** — every attempt ran real turns (no 0-turn faults).
Not comparable with the omp `.16` cell (ticket 7): harness, effort and rules
all differ. Tickets 0-5 passed; s6/s7/s8 failed with regressions [0..5]: the
model's transaction/aggregate/join changes crashed every SELECT (`ERR syntax
internal error: 'list' object has no attribute 'lower'`, reproduced by the
reviewer against the s8 commit; the stage-5 code answers correctly), so suite
0 and the stage-4 sentinel failed first. s9 read 6/7, s10 was skipped (needs
9), and s11 failed 0/7 then 6/7 (CREATE INDEX still unparsed in both). The
quarantined cells `poisoned_campaign_…_103729` (R1) and `…_122342` (stale
shell key, opencode 401 at round 0) were restored to their `campaign_` names
and flagged through `POST /api/campaigns/:id/poison` (round-14 N1).

**Round-14 review fixes (2026-10-18, rules still `2026-10-18.17`)**: N1 —
campaign cells are poisoned/hidden in their own folder
(`portal::set_campaign_flag`, `POST /api/campaigns/:id/poison|portal` with a
reason), honoured by the portal and the console board. N2 — a harness fault
(the agent never started after one retry, or a provider billing/auth trip)
stops the cell (`rollup.harness_fault`, stop reason "harness fault") and the
runner poisons it, instead of a `fault` square that blocks `needs` in a
published cell. N3 — "never started" is the runner's sign-of-life flag
(`OmpExecutionError.started`: a session file, stdout, a turn or tokens), not
`steps_taken == 0`; billing errors are classified on both paths, never
retried; a repair-round agent that never starts is retried once, then ends
the repair loop. N4 — opencode no longer counts the turn killed at the cap
(the repair round got one turn less).

**Round-14 addendum, G1 (2026-10-19, rules still `2026-10-18.17`)**: since
`.17` a suite grades only the tickets the ladder passed, but ticket 4's
persistence scenario still built its dataset from `Profile::through(4)` and
ran whenever the suite's highest stage was >= 4; the upgrade sentinel was
gated on `top >= 4` and built from `Profile::through(top)`. Live damage: the
ling `.17` cell (`_154400`, tickets 1-3 not passed) failed ticket 4 on a
column-list `INSERT` and `ORDER BY ... LIMIT`, and is poisoned; the
already-poisoned DeepSeek `_103729` failed tickets 4 and 9 the same way; the
DeepSeek `_142825` cell lost its stage-8 sentinel (aggregates, ticket 7
failed). Now `driver::persistence_profile` (tickets 0-4 ∩ passed, lite
narrowed) feeds the scenario, which runs only when ticket 4 is in the suite;
`driver::sentinel_profile` gates and shapes the sentinel; without `ORDER BY`
sentinel reads are kept unordered (`SentQuery.ordered`, serde default
`true`) and compare as a multiset of rows. With tickets 0-4 passed the
dataset is byte-identical to before, so no recorded grade of an unpoisoned
`.17` cell changes (`_142825` would also have had its stage-8 sentinel
re-read at tickets 9 and 11, both failed on their own stage): the rules stay
`.17`, and the next bump goes with the N5 raw-answer feedback and the
ticket-11 work.

**Round-11 review fixes landed (2026-10-18, rules still `2026-10-18.17` — no
cell has run on it)**. **Host ops (O1), read this before starting anything**:
the engine and GUI are systemd units (`subdollarbench-engine` on :3000,
`subdollarbench-gui` on :3001). After every rebuild run
`sudo systemctl restart subdollarbench-engine subdollarbench-gui`; never
start `./target/release/subdollarbench ui` by hand. A hand-started pair held
the ports while both units crash-looped ~19k times (each retry ran the queue
pump, the reconcile sweep and the OMP updater before a failing bind), and the
served page silently went stale. `UiServer::start` now binds first and exits
with `cannot bind … (held by pid N)` before any side effect (unit-tested via
`UI_STARTS`), and the unit drop-ins add `RestartSec=10` +
`StartLimitIntervalSec=300`/`StartLimitBurst=5` so a misconfiguration stops
instead of looping. Rest of round 11: (1) the cap clamp converts *effective*
headroom into the API dollars OMP enforces, using the cell's observed
api/effective ratio (0.25 before the first attempt) — a 3.4x floor can no
longer turn $0.05 of headroom into a $0.17 overshoot; the floor stays in
effective dollars; (2) `over_budget` (spent ≥ cap) and `cap_reached` (floor
stop below it) are separate flags, labelled `over budget by $X` vs
`budget cap reached` on the Run header and `budget cap` on the portal; (3)
the four remaining prompt openers ("Aggregates (ticket 7) read one table",
…) name a ticket as contract instead of claiming it passed; (4) the Cause
clamp moved to an inner div so the td stays a table cell; (5) an interrupted
ticket reads `interrupted` (not `running`/`not reached`), the stalled wall
bar measures start → finished_at/last activity, and stalled stamps carry the
date when it is not today (`17 Oct 16:16`); (6) the Admin legacy controls
(review judge, single run + prompt editor, run queue) sit in one "Legacy (not
campaigns)" disclosure, with effort/harness left as the campaign's controls.
O2: `log_mtime` (the newest console log's mtime) joins `updated_at` and
`live.last_activity_at` in the stalled-since derivation, and a cancelled
campaign writes `state: "cancelled"` + `stop_reason: "on request"` on its way
out.

**Round-10 harness fixes landed (2026-10-18, D1-D5, RULES 2026-10-18.17)**:
(1, D1 HIGH) the per-round `FEEDBACK_REPORT.md` is now deleted before every
attempt's first turn and again after its verification, is ignore-listed in
the workspace `.gitignore` the harness rewrites, and `git rm --cached`s a
legacy tracked copy — ticket 1's stale "0 of 2, undefined: coerceNum" report
used to be committed by five later tickets and believed by the model. (2,
D2 HIGH) a failed or skipped ticket no longer reads as contract:
`render_spec_md` collapses its section to a FAILED/SKIPPED marker line, the
log header prints the **effective** acceptance list, and every attempt after
a failure carries a harness note on its prompt (failed/skipped tickets
named, the rollback point, the `accepted_for` suite) — generated from the
same list the verifier runs, so it cannot drift; the static prompts' "Sorting
and paging work (ticket 2)" / "suites still run (tickets 0-9)" claims became
outcome-neutral. (3, D3) an attempt's budget is clamped to
`min(ticket budget, cap − spent)`, and the ladder stops as `budget cap` when
the remaining headroom is under `BUDGET_FLOOR_USD` ($0.02) — the DeepSeek
cell overshot $0.75 by $0.0184 on a $0.0115-headroom lite attempt. (4, D5)
`live.first_fail` is one line, ≤160 chars (`campaign::truncate_line`, the
same rule as `FailureSummary.line`). Tests: SPEC markers, preamble naming +
suite-from-`accepted_for` (incl. a lite retry), the id formatter, the
truncation, the budget floor + clamp with a mock executor, and a git test
proving a legacy tracked report never re-enters `git ls-files`. D4 (the
compute floor dominates cheap cells' cost) is an operator decision, not a
code change.

**Round-10 review fixes landed (2026-10-18, browser pass)**: the console was
driven with Playwright 1.58 at 1440 and 390 px; nothing mutated. (1) elapsed
times are honest: the rollup's `finished_at` is empty until the run ends
(mid-run status carries it blank), a running cell shows `started → running ·
Xm elapsed` (recomputed on the 4 s board poll), a stalled one `stalled since
HH:MM · in ticket N`, and the stop reason colours only done cells. (2) the
Board and Run tables become stacked cards under 700 px: `scrollWidth ==
innerWidth == 390` on all four tabs, and the header subtitle is dropped
there. (3) the Live stream is quiet by default: a new `telemetry` kind
(OMP / TELEMETRY / TOKENS / LIVE Turn / WAITING) is off in the chips (now
real buttons with `aria-pressed`), `[WAITING]` keepalives collapse to one
`turn N · thinking · Xs` line, `[THOUGHT]`/`[RESPONSE]` render as prose,
`[ACTION]` + its output fold into one `<details>`, verdict/test-result lines
become coloured banners, and the duplicate summary line under the card is
gone — the card is the summary. (4) the first paint is the log **tail**
(`tail=400`), not the whole file: a finished cell's 18k-line log used to
render 3.7 MB of HTML. (5) every ladder square carries its ticket number and
one legend serves all strips. (6) the header's five badges collapse into one
health dot whose tooltip lists the versions and integrity; "Rebuild portal"
moved to Admin. (7) the campaign launcher reads the picker's model and the
main effort selector (one control each), shows the resolved model/effort and
disables itself while a cell runs (the server-side lock is the backstop).
(8) polling needs a visible page, the queue poll only runs on Admin, and the
tab title tracks the live cell (`▶ t11 · 9/30 · $0.12 — SDB`). (9) a11y:
tabs are `role=tab` with `aria-selected`, rows are keyboard-openable, chips
are `aria-pressed` buttons. The one-lock-holder, honest-state and tail-fetch
fixes were verified against the finished DeepSeek cell in the browser.

**Round-9 review fixes landed (2026-10-18)**: (1) the live block lives in
memory in `CampaignLogger` and status.json (which now carries the whole
rollup) is read-modify-written only when a write is due — at most every
5 s plus verdicts — instead of twice per log line (cell 8's ~17k lines
meant gigabytes of JSON parsing under one mutex). (2) "last check" is
honest: `check_pass`/`check_total` counters plus the **first real
failure** of the current verification (reset on `[VERIFY]`, a ticket or
repair header), the pass/total summary from the FEEDBACK lines, so a
skipped tail is no longer shown as the cause; unit-pinned from real
lines. (3) the header labels the provider-reported spend `ticket API $`
against the effective-cost budget and `campaign $ … (effective)`. (4)
the lock-conflict message lost its 30 literal spaces, `last_activity_at`
is RFC 3339 and the console renders "4s/12m ago", and the real-log kind
check is commented as the suite's one intentional runs/ read. Console
improvement B landed with them: a Live header card (turns, ticket API $,
wall and campaign-$ bars, the last check, the staleness colour amber
> 2 min / red > 5 min, and a pulsing ladder square) and a stalled cell no
longer marks its current ticket `running` or reports a stop reason.

**Round-8 review fixes landed (2026-10-17)**: (1, MEDIUM-HIGH) the suite
no longer touches the real `runs/` directory: `resolve_runs_dir()` honours
`SDB_RUNS_DIR` and every web/campaign test runs against an isolated temp
dir (a guard test proves a live-looking lock is byte-identical after the
requests; the two lock tests serialize on a test mutex). Verified while
the muse cell held the real lock: it survived the suite untouched. (2) the
launcher argv lives in `campaign::launcher_args` and a bin test parses it
with the real `Cli`; the spawn target resolves the repo's release binary
(a rebuilt binary's `/proc/self/exe` reads `… (deleted)`), and the two
stale launch logs are gone. (3) the child gets its own process group
(`process_group(0)`), so restarting the console cannot kill a campaign.
(4) `effective_state` compares the lock's campaign id with the cell's,
not "any lock". (5) the lock stores the holder's process start time
(pid-reuse proof) and the 409 says how to clear a wedged lock.
(6, HIGH) `log_kind` strips the real `[HH:MM:SS] ` timestamp and maps
tool/prose continuations: the default Live chips went from **0 to 1493
lines** on the muse cell's live.log, pinned by verbatim timestamped lines
plus a real-log assertion. (7) the Live strip reads the runner's live
block (or a timestamp-anchored fallback — verified against the real log:
ticket/turn/spent/round all match) and the `$`-less `spent=` pattern is
fixed. (8) `GET /api/campaigns/:id` answers 200 with the campaign's full
ladder (the current ticket `running`) whenever the directory exists — no
more "run not found" during ticket 0. (9) the board key is a total order
with rollup-less live cells pinned first; ling 5.8 ranks above muse 4.0
again and the sort cannot panic. Improvement A landed: the runner
merge-writes a `live` block (ticket/title/type/variant/round/phase/
turn/ticket-spent/verdict/last-activity) into status.json every 5 s and
immediately on verdicts/phase changes; the Board/Run/Live headers read
it (the strip falls back for pre-existing cells like the muse one).

**gui-spec.md implemented (2026-10-17)**: the console is campaign-first.
Phase 1 (`3f7dd2d`): manifests record one `CampaignRound` per
verification (stage results, turns, cost estimate, candidate-log tail);
`StageRecord` carries `rounds` + the §5.2 classified `failure`
(fault / no_start / regression / late_check / own_stage / agent); the
ladder (§5.3) and `stop_reason` live in one Rust function; the rollup
records its caps; `GET /api/campaigns` carries each cell's ladder and
stop reason, `GET /api/campaigns/:id` the full detail with backfilled
rounds (lenient manifest read + per-round scores recovered from the
stage console log, stderr evidence included), and the log endpoint grew
`?after=` byte offsets and `?kinds=` filtering. Phases 2-6: the console
is four tabs — **Board** (the default: one row per campaign cell with a
status-coloured ladder mini-strip, model/harness/effort grouping with
`×N runs`, a rules-version filter with an "older rule sets" divider,
stop-reason states), **Run** (header, spend bar with the cap marker, the
full ladder table with per-round scores and `⚠` regressions, one-line
causes that expand to the full verifier error and stderr tail, links to
the run console), **Live** (filter chips, 4 s incremental `?after=`
polling, prompt/report block collapsing, follow toggle), and **Admin**
(a campaign launcher posting to `POST /api/campaigns` — spawns what
scripts/sql-cell.sh runs and refuses a busy engine with 409; the
single-run launcher moved behind "Advanced — single run (not a
campaign)"). The retired Leaderboard/Models/Tasks/Matrix/Runs/Console
and Campaigns-list panels and their JS are gone; one ranking
(`campaign::board_order`: current rules, points, points/$, cheaper)
serves the API, the console and the public portal, which also renders
the shared ladder strip. Acceptance checked against cell 8: ticket 7
`0/6 → 5/6`, ticket 8 full `5/6 → 2/6` (regression), lite `5/6 → 0/6`
with `server didn't start: server.py:101: IndentationError…`, tickets
9/11 `not reached: budget cap`, 10 `skipped: needs 7`; the console JS
passes `node --check`, the served page carries no retired-task names,
and the full lib suite is 873 green.

**Round-6 review fixes landed (2026-10-17)**: (1) `pick_other_table`
actually landed this time — a subquery's inner table is never the outer
statement's, in the scalar, `EXISTS` and `IN (subquery)` arms, verified
by grep (4 sites) and by the agreement gate; the stash's `allow_sub =
false` was not needed (soak 0/3000). (2) The perf gate runs on a fresh
**scratch store** (the crash-ladder pattern without the copy) and
restores the real store afterwards, so its two 200k-row tables never
touch a candidate's own data; the mid-gate restart is the ticket-4
*graceful* one (SIGKILL is ticket 5's contract). (3) The gate measures a
round-trip **floor** on a one-row table on the same connection and
accepts an index at <= 2x it without the 10x ratio, so a compiled engine
with a fast scan is not punished for the network; pinned by two
fake-engine unit tests (fast scan passes, slow index still fails). (4)
The public board ranks by **difficulty-weighted points** (the proposal's
headline), then points per $, then cheaper; reached and regression-free
stay as columns, rollups carry `rules_version` and stale-rule cells group
below current ones, and cells whose stage runs are poisoned or hidden are
never published. (5) `repair --apply` and the review command route
through the sdb-sql board instead of overwriting `portal/` with the
legacy leaderboard. (6) `inside_transaction` can emit index DDL and the
generator snapshots its index registry at `BEGIN` and restores it on
`ROLLBACK` (a rolled-back `DROP INDEX` must not free a UNIQUE column for
swaps); re-soaked, 0/3000. Nits: the (a) promotion no longer pushes
`acked` twice, the registry comment records that a failed `CREATE UNIQUE
INDEX` stays registered (conservative), and the prompt/board no longer
say "sqlite file format". Gates: agreement gate + mutants through 11,
soak 0/3000, perf unit tests, the reference perf test, the 20/20 bar, and
both stage-11 Docker live proofs (the full one now asserts the scratch
line). RULES `2026-10-17.15`; the `.14` entry was corrected to say what
it actually graded.

**Round-5 review fixes landed (2026-10-17)**: bugs A/B/C from the
stage-10 soak (111/3000 diverging seeds -> **0/3000**), verified by the
soak itself: (A) the toy's `INTEGER PRIMARY KEY` auto-assign is now
`max + 1` even for negative maxima (SQLite verified: -39 then NULL ->
-38; the old `.max(1)` gave 1) and `i64::MAX` errors loudly; (B) an
`INSERT ... SELECT` caps a copied INTEGER key below 2^62 (SQLite picks
*random* rowids once the max is i64::MAX, so the expectation was
nondeterministic); (C) its user predicate is parenthesized, so the
`pk IS NOT NULL` guard cannot be bypassed by a top-level `OR` (NULL keys
would auto-assign in query-plan order); (3) the `SET a = b, b = a` swap
never touches a UNIQUE column (SQLite checks per row in rowid order, a
candidate may check in insertion order: scan order is not dialect); (4)
the (a) in-flight row is promoted from `maybe` to `must` once it is
visible after the first restart (round 4 had claimed it); (5) the soak
derives its depth from `driver::MAX_DIFFERENTIAL_STAGE`, so it cannot
fall behind a ticket again, and "the soak is green through stage N" is a
listed gate from here on; (6) RULES `2026-10-17.13`, the join-select
conjunct is parenthesized, and the "could not signal the other
processes" warning is skipped when PID 1 already exited. Gates: the
stage-10 **soak is 0/3000**, the agreement gate and 109 verifier tests
pass, fmt/clippy clean. **Next**: re-apply the ticket-11 WIP (`stash@{0}`)
on top, re-enable round-4 finding 3, then the toy index engine + perf
gate + campaign/proofs, and a stronger-model cell through tickets 4-10.

## Coverage campaign (operator requirement: bench code >= 80% lines)

Measured 2026-10-14 (`cargo llvm-cov --all-targets --summary-only`, the
same command the CI gate runs): **84.65% lines** (regions 85.47%,
functions 88.74%), up from **61.03%** before the campaign. The operator
requirement (bench code >= 80%) is met with margin; the CI gate at 80
enforces it when Actions is re-enabled.
Note: the command counts test code in both numerator and denominator;
production gaps were tracked alongside each wave and the big files were
covered for real (see per-file numbers below).

Wave log:
- Wave 1: pubsub 31->95%, pdf 35->96.6%, proxy 58->88.4%, wal 98.3%,
  binproto 98%, judge 48->95.7%, review_cmd 0->96.9%, repair 7->90.9%,
  cost_annotate 28->57% (+ the total_cost_usd gate fix), CLI handlers,
  web/server 48->77.3%, web/queue 35->67.7%.
- Wave 2 (aggregate 61.0 -> 76.6%): the two web files above + CLI.
- Wave 3: docker 47->85.7%, batch_ref 32->80%, leaderboard 92.2%,
  portal 98.7%, remote.
- Wave 4: pipeline +139 production lines (eval-only mock-candidate
  flows), opencode 95.4%, omp 95.8%, omp_updater 96.5%.
- Wave 4b: main.rs 68.2->81.5%, main_campaign.rs 10.9->69.6%.
- Aggregate after 4b: **84.65% lines** — target met.
- Wave 6 (stage-2 wave, 2026-10-15): aggregate 84.65 -> **84.71% lines**
  (regions 85.55%, functions 88.79%). The eval-only live proof covers the
  stage-14/15/16 orchestration and the disk/compaction verifiers
  (verifier/redis.rs 78.78 -> 82.11% lines; binproto 93.78% incl. the
  admin stage; sandbox/docker.rs 85.39% with the live bind round-trip).
  Gate margin ~4.7pp.
- Wave 7 (sdb-edge proxy campaign, 2026-10-15): aggregate 84.71 ->
  **84.43% lines** (regions 85.27%, functions 88.74%). The dip is the new
  proxy stage-6..9 orchestration blocks (live-only) and the docker-gated
  stages 6/7; verifier/proxy.rs sits at 79.65% lines (cache/limits/status
  logic is mock-covered, pause-based stages need live upstreams),
  campaign.rs 91.73%, pipeline.rs 56.59%. Gate margin ~4.4pp.
- Wave 8 (sdb-edge full-ladder live proof, 2026-10-15): aggregate 84.43 ->
  **84.82% lines** (regions 85.69%, functions 89.09%). The eval-only
  reference-proxy run covers the proxy arm's stage-6..9 orchestration and
  the fixture (pipeline.rs 56.59 -> 59.12%); gate margin ~4.8pp.
- Wave 11 (sdb-http live proof, 2026-10-15): aggregate 84.52 ->
  **84.76% lines** (regions 85.55%, functions 88.26%). The eval-only
  reference-file-server run covers the http arm's campaign stages and the
  capped restart path (verifier/http.rs 76.80%). Gate margin ~4.8pp.
- Wave 10 (sdb-http campaign, 2026-10-15): aggregate 84.76 ->
  **84.52% lines** (regions 85.31%, functions 88.18%). verifier/http.rs at
  76.79% lines (the five new stages + the full mock/defect matrix are
  covered; the arm's live-only restart paths and pre-existing gaps
  remain), campaign.rs 91.82%, queue 85.77%. The recurring
  `test_bench_lock_released_on_drop` flake under full-suite load is fixed
  structurally: the test now probes a private lock path (the shared bench
  lock is legitimately held by live pipeline tests running in parallel).
  Gate margin ~4.5pp.
- Wave 9 (sdb-queue campaign + live proof, 2026-10-15): aggregate 84.82 ->
  **84.76% lines** (regions 85.56%, functions 88.88%). The new
  verifier/queue.rs lands at **85.77% lines** (mock broker + defect
  matrix + both durability halves + the eval-only pipeline run);
  pipeline.rs rises again with the queue arm and fixture. Gate margin
  ~4.8pp.

Remaining smaller residuals if a future wave wants more margin:
web/server.rs (1,098 incl. test code), web/queue.rs (798),
pipeline.rs (~1,100 prod), verifier/redis.rs (514/578),
verifier/http.rs (244), report/leaderboard.rs (263), cost_annotate,
tool_normalizer, portal_proxy, cutoff_annotate.
Scheduling note: live-docker tests can flake under heavy host load
(parallel agent builds); the suite is green on a quiet host. If CI shows
flakes, serialize live-docker tests with a shared test lock.

## Grid log (sdb-edge, omp, high effort)

- **ling-3.1-flash** (spec tickets 0-6, 2026-10-15 run 1,
  `campaign_sdb-edge_openrouter_inclusionai_ling-3.1-flash_20261015_101524`,
  RULES `2026-10-15.2`, $0.80 cap, 29 min): **reached 4 as recorded
  (reached 3 under the fixed harness — see below)**, $0.1907, 6 tickets
  attempted. t0 genesis PASS first try (stage 1; 4482 req/s candidate
  bench). t1 balance PASS (stages 1,2,3). t2 failover **FAIL -> repaired
  in round 1** (stage 4 read timeout against a frozen upstream; the repair
  added upstream timeouts + retry). t3 quick-failover PASS first try
  (stage 6) — the t2 repair generalized. t4 health PASS in-run (stage 7)
  but **luck**: the verify-time pool churn (harness bug (b) below) had the
  candidate's prober already in unhealthy-recheck mode, which masked that
  it never probes *healthy* upstreams. t5 caching FAIL x2 in-run (stage 8
  read timeout = harness bug (a)), not a candidate failure.
  Clean-harness replays (eval-only, no API cost, fixed binary):
  t4 state -> t0-t3 PASS, **t4 FAIL stage 7 honestly** ("upstream2 was
  down for 5s of idle but /status still says up — probe upstreams
  actively"); final state -> **stage 8 (caching) PASS**, **stage 9 FAIL**
  ("a half-sent request stayed open for 12s: enforce a header/read
  timeout (slow-loris)" — the oversized-header/body rejections passed).
  Honest capability gaps: active health checks (t4), slow-loris timeout
  (t6); caching is correct. Rejections passed: nothing on t0-t3.
- **nemotron-3-super-120b-a12b:free** (spec tickets 0-6, 2026-10-15 run
  1, `campaign_sdb-edge_openrouter_nvidia_nemotron-3-super-120b-a12b_free_20261015_111858`,
  RULES `2026-10-15.2`, $0.50 cap, ~20 min): reached **none** — t0 **FAIL
  0/1** (recorded $0.0329, 24 initial + 12 repair turns; ~74k
  prompt tokens). Its agent tooling worked this time (`write`/`bash` OK,
  unlike the sdb-kv cell) and it produced a 234-line `proxy.py` with
  round-robin + X-Forwarded-For, but the verifier cannot parse its
  response in either round ("Failed to parse HTTP response from
  127.0.0.1:8091"): the hand-rolled forwarder echoes upstream bytes yet
  emits a malformed/partial reply (response-layer failure, not routing).
  Cost nit: the `:free` variant still lands a ~$0.03 cost in the record
  (same as its sdb-kv cell, $0.0312) — the pricing path does not zero
  free-tier variants; open item (either zero it or label the rows as
  token-only).
- **inkling-small:free** (sdb-edge run 1, 2026-10-15,
  `campaign_sdb-edge_openrouter_thinkingmachines_inkling-small_free_20261015_122552`,
  RULES `2026-10-15.2`, $0.50 cap, ~7 min wall): reached **1**
  (2 regression-free tickets), recorded $0.0263 (the free-tier pricing
  nit again). t0 genesis PASS first try (21 turns, $0.0081; left
  `proxy_temp.py`/`proxy_temp2.py` copies in the workspace). t1 balance
  PASS (18 turns, $0.0070; rewrote `proxy.py`). t2 failover **FAIL 1/4**
  (30 turns, $0.0112) with **regressions [2, 3] (sev 3)** — the cell's
  headline finding: the initial attempt had no upstream timeout/retry
  (stage 4: "GET 2 during failover: Read timed out"), and the **repair
  round broke round-robin (stage 2: unparseable response) and keep-alive
  (stage 3: read timeout)**. First repair-induced regression on routing
  stages in the sdb-edge grid; exactly what cumulative acceptance exists
  to catch (the model's own `proxy.py.bak` was 307 lines vs the 272-line
  rewrite).
- **ling-3.1-flash** (sdb-edge run 2, 2026-10-15,
  `campaign_sdb-edge_openrouter_inclusionai_ling-3.1-flash_20261015_121246`,
  RULES `2026-10-15.2`, $0.80 cap, ~22 min): reached **none** — t0 **FAIL
  0/1 both rounds** ($0.0518, 24 + 12 turns). Symptom: the verifier's
  `GET /` connected but never received a framed response (8s exchange
  deadline) in either round, while the candidate's own direct fetches to
  all three upstreams returned 200s and its log shows a threaded proxy
  listening on 8091 (401-line `proxy.py`: thread per connection,
  read/parse/forward with re-framing). => a **client-request-path defect**
  (connect-but-don't-answer for the verifier's plain `GET /` with
  `Connection: close`), not transport or upstream access. Run-to-run
  variance on the genesis ticket is now observed for this model: run 1
  (same spec) passed t0 first try and reached 4 recorded.
  Frontier-note for prompts (proposed, NOT applied — it is a spec change):
  the candidate container carries an `http_proxy` env var pointing at an
  egress proxy; the agent burned ~5 turns discovering `curl` was routed
  through it (`--noproxy '*'` fixed it) although raw sockets were
  unaffected. A prompt line ("tool curl may honor an egress-proxy env
  var; your server must dial `upstreamN` with raw sockets") would remove
  harness noise; applying it means a spec bump.
- Harness bugs this cell found (all fixed, same day):
  (a) candidate restarts did not re-attach to the run network -> a
  restarted proxy could not resolve `upstreamN` (getaddrinfo stall; stage
  8 read timeout). Restart blocks now call
  `connect_candidate_to_network()`.
  (b) the verify-time pool check used **host-published ports** (a harness
  convenience with byte-identical in-process fallbacks) and rebuilt the
  pool before *every* verification — killing the candidate's pooled
  connections and health state right before routing stages. In eval-only
  replays this produced spurious stage-2 `[0,20,10]` / stage-6/7 failures.
  Pool health is now a container-state check (`upstream_container_running`)
  and the pool is rebuilt only when missing/stopped; the misleading
  "unreachable at verify; recreating fresh" churn is gone.
  (c) the proxy bench tried the live-nginx calibration (`ref-http`) even
  though the denominator is always discarded for proxy tasks (misleading
  "[BENCH] live Nginx reference FAILED"); the calibration is now skipped.
  Open question (hygiene): in this environment the upstream containers'
  host publishes (8101-8103) are configured but sometimes have no
  listener (manual `docker run -p` works). Nothing in the graded path
  depends on the host mirrors any more (candidates dial the network
  aliases; fixture markers have in-process fallbacks).

## Grid log (sdb-http, omp, high effort)

- **ling-3.1-flash** (spec tickets 0-7, 2026-10-15 run 1,
  `campaign_sdb-http_openrouter_inclusionai_ling-3.1-flash_20261015_155225`,
  RULES `2026-10-15.4` semantics in a **stale binary** (see below), $0.80
  cap, ~30 min): **reached 6 — seven tickets first try, one stage short of
  the full ladder**, $0.1560, tickets/$ 44.9, 7 regression-free. Per
  ticket: s0 root+404 PASS 8t ($0.0133) · s1 echo/UA PASS 9t ($0.0079) ·
  s2 files PASS 16t ($0.0204) · s3 ranges PASS 12t ($0.0155) · s4
  validators PASS 12t ($0.0144) · s5 gzip PASS 12t ($0.0157) · s6 chunked
  uploads + limits PASS 18t ($0.0273, under the 128MB cap) · s7 hardening
  **FAIL 9/10** 30t ($0.0415).
  The t7 story has two layers: the INITIAL failure was the *stale binary's*
  strict garbage-line rule ("a malformed request line must answer 400 (or
  close), got 405" — RULES `2026-10-15.4` wording; the candidate's 405 is
  accepted under `2026-10-15.5`, which was committed but not rebuilt into
  the release binary before the cell launched). The model then spent its
  repair round on that report and **broke the 8KiB head cap** — the final
  state answers **200 to a 32KiB header block** ("32KiB of headers must be
  refused (431/400/close), got 200"), so the recorded verdict is a genuine
  stage-10 failure (the third repair-induced regression in the grid, after
  inkling-small's [2,3] and ling-edge run 1's spoiled t5). None of the
  replayed-under-current-rules outcome can rescue the final state; the
  pre-repair snapshot was squashed into the per-ticket commit.
- **ling-3.1-flash** (sdb-http run 2, 2026-10-15,
  `campaign_sdb-http_openrouter_inclusionai_ling-3.1-flash_20261015_163332`,
  RULES `2026-10-15.5`, $0.80 cap, ~40 min): reached **4** (5
  regression-free), $0.1183, tickets/$ 42.3. t0-t4 PASS first try
  (19/16/18/12/12 turns, $0.0120-$0.0216); t5 gzip **FAIL 5/8** (24 turns,
  $0.0335) with **regressions [6, 7]** — adding compression broke
  **suffix-range support** (`Range: bytes=-4` -> `200` instead of `206`)
  on a stage that had passed at ticket 3. The single repair round (12
  steps) failed to fix it: the model could not reproduce the failure
  locally ("my local file test works..."), and its repair reasoning even
  guessed the wrong endpoint. Third model-caused regression in the grid.
- **Variance read (same model/app/spec)**: ling on sdb-http = **reached 6
  (run 1) vs 4 (run 2)** — a two-ticket spread with different failure
  modes (run 1: a repair broke the 8KiB head cap at t7; run 2: an edit
  broke suffix ranges at t5, and the repair failed). Reinforces the
  "sample more than once per (model, app)" rule for the paper's curves.
- **Launch hygiene (learned twice)**: rebuild `target/release/subdollarbench`
  after ANY grading/harness change before launching cells — a stale binary
  spoiled ling's sdb-edge-run-1 t5 verdict and this cell's t7 initial
  attempt. (Add to the run checklist.)

## Grid log (sdb-queue, omp, high effort)

- **ling-3.1-flash** (spec tickets 0-8, 2026-10-15 run 1,
  `campaign_sdb-queue_openrouter_inclusionai_ling-3.1-flash_20261015_124730`,
  RULES `2026-10-15.3`, $0.80 cap, ~30 min): **reached 8 — the FULL ladder
  (9/9 tickets, 9/9 stages, zero regressions, zero repair rounds)**,
  $0.2412, tickets/$ 37.3. Every ticket passed on the first attempt:
  s0 genesis 24t ($0.0300) · s1 leases+ownership 18t ($0.0241) · s2
  nack/delays 18t ($0.0442) · s3 DLQ 18t ($0.0189) · **s4 durability
  (prepare -> SIGKILL -> restart -> verify) 18t ($0.0345)** · s5
  concurrency (6 consumers / 200 jobs) 12t ($0.0296) · s6 STATS 12t
  ($0.0205) · s7 delays+redrive 12t ($0.0160) · s8 hardening under the
  128MB cap 18t ($0.0234). This is the benchmark's **first complete
  survival curve** and the first time the destructive durability stage
  ever ran against a model candidate — and the first sdb-queue cell ever.
  Cross-app read: the same model reached 4/3-honest on sdb-edge and 4 on
  sdb-kv (both ladders wall earlier), so the queue ladder is materially
  easier for it — small line protocol, no RESP/HTTP semantics — which is
  exactly the per-app difficulty spread the portfolio exists to measure.
- **inkling-small:free** (sdb-queue run 1, 2026-10-15,
  `campaign_sdb-queue_openrouter_thinkingmachines_inkling-small_free_20261015_162803`,
  RULES `2026-10-15.5`, $0.50 cap, ~4 min): reached **none** — t0 **FAIL
  0/1 both rounds** ($0.0102, 36 turns). Pure tool-hygiene failure: its
  300-line `server.py` does not parse (`IndentationError: unexpected
  indent` at line 182, unchanged through the repair round) so nothing
  ever listened on 6385 and both rounds died with **connection refused**.
  A single `python3 server.py` (or `py_compile`) check would have caught
  it; the model spent all 36 turns writing/editing without executing its
  artifact. Failure-taxonomy gold: the ladder's stages never even ran.
  Also the spread data point for the queue ladder: a second model does
  NOT get anywhere near ling's full-ladder run.
- Next for this campaign: more models (free tier) for spread, and the
  deferred depth tickets once a second model saturates (the ladder is
  complete for ling, so cost/survival comparisons need other samples).

## Grid log (sdb-kv, omp, high effort)

- **nemotron-3-super-120b-a12b:free** (spec 0-10, 2026-10-15 run 1,
  `..._043542`, RULES `2026-10-14.13`): reached **none** — s0 **FAIL
  2/3** ($0.0312, 28 initial + 12 repair = 40 turns), 0 regression-free.
  Two failure layers: (1) its C server's `ht_get` returns `kv_t*` but
  every caller treats it as `char*` -> segfault under the verifier
  (stages 1-2 pass, 3-4 time out after the crash, no pubsub; advisory
  102/103 die on connection drops); (2) **it could not operate the
  agent's bash tool** — after one successful call, every later `bash`
  invocation was rejected ("Service mode does not accept async or
  timeout; use ready.timeout for readiness") and it burned most turns
  guessing argument shapes instead of testing its code. Genuine model
  failure: harness-tool operation is part of the measured skill; the
  tool behaved per spec (early calls worked). Scheduled-cell mishap
  before this run: the 00:05 timer shell was GC'd overnight; relaunched
  manually at 04:35. **The 3-model grid now has cells for all three
  models** (ling best: reached 4).
- **ling-3.1-flash** (spec 0-10, 2026-10-14 run 12, `..._181711`, RULES
  `2026-10-14.13`): reached **0**. s0 PASS 3/3 ($0.0539, 28t, C). s1
  **FAIL 0/4** ($0.0607, 13+12, regressions [1,2,4] sev 3): the model
  rewrote `server.c` to add SET-options + pub/sub but dropped the RESP
  parsing section in the rewrite (build: undefined `parse_command`); the
  repair's reinsertion landed mid-`dispatch` (split function) and its
  turn budget went to edit-tool syntax friction trying to cut/paste the
  block; final build broken -> all stages refused. **Compliance fix
  verified live** (no false DQ; the candidate used a `/dev/tcp` probe,
  and client-mode nc/socat are now allowed by construction). $0.1145,
  tickets/$ 8.7.
- **ling-3.1-flash** (spec 0-10, 2026-10-14 run 11, `..._180418`, RULES
  `2026-10-14.12`): s0 PASS 3/3 ($0.0283, 19t, Python). s1 **invalidated
  by an anti-cheat false positive**: the model's own test script piped a
  RESP payload into `nc -q1 127.0.0.1 6379`, and the scanner flagged
  `nc` in any mode -> instant DQ (0/4). Client-mode nc cannot implement
  or tunnel a service; the cheat vector is LISTEN/EXEC mode. Fixed same
  day (RULES `2026-10-14.13`): only listen/exec forms fail, client-mode
  is allowed; tests cover both. Do not quote this cell; re-run replaces
  it.
- **ling-3.1-flash** (spec 0-10, 2026-10-14 run 10, `..._172751`, RULES
  `2026-10-14.12`): **reached 4 — new best; first run past s3.** s0 3/3
  ($0.0265, 28t), s1 4/4 ($0.0260, 13t), s2 5/5 ($0.0484, 13+12 repair),
  s3 durability **PASS 6/6 ($0.0675, 36t — the full pool)**: the Q2 raise
  (30->36) was decisive (four prior 30-turn attempts died in this class).
  s4 ENOSPC **PASS 7/7 ($0.0408, 30t)**: **stage 8's pass path executed
  live for the first time** — the fault container ran and the model's
  `SDB_DATA_DIR` + honest write-error implementation passed. s5 INFO
  **FAIL 0/8** ($0.0486, 13+12, regressions [1..6,8] sev 3): the initial
  attempt correctly failed stage 7 (`-ERR unknown command 'INFO'` —
  **stage 7's first live execution**, verdict machinery correct); the
  repair round botched the build (mangled `del`, unused imports, missing
  return) and ran out; re-verification refused. $0.2578, tickets/$ 19.4,
  5 regression-free. Harness clean (anchor, refs, ENOSPC lifecycle).
- **LADDER COMPLETE (2026-10-14 evening):** all 11 tickets built —
  stages 1-13. Ticket 10 (3-node HA, RULES `2026-10-14.12`) per operator
  decision Q1: automatic failover (T=10s, deterministic leader), rejoin
  pointed at the new leader, accumulated RESP suite + ./test.sh against
  the current master; partition safety deferred. Validation: `ha_node_args`
  unit test + ROLE/probe helper mock tests; live execution needs a model
  that passes t9 (none has yet). Spec total: 11 tickets, \$1.55 sum.
- **ling-3.1-flash** (spec 0-9, 2026-10-14 run 9, `..._170557`, RULES
  `2026-10-14.10`): reached **0** — first cell to fall at s1. s0 3/3
  ($0.0395, 28t), s1 expiry **FAIL 3/4** ($0.0430, 13 initial + 12
  repair). C this time. s1's initial attempt left `SET ... PX` as an
  unknown arity (`argc != 3`) and `entry_expired` defined but never
  called; the repair round diagnosed both correctly, started the pub/sub
  and expiry edits, then burned turns on edit-tool line-anchor churn
  (struct redefinition rejected twice) and hit the cap with none of the
  fixes landed; re-verification unchanged (3/4). $0.0826, tickets/$ 12.1,
  1 regression-free. Harness clean.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 8, `..._162654`): reached
  **2** — s0 3/3 ($0.0316, 28/28 — used the full new initial cap), s1 4/4
  ($0.0319, 13/13), s2 5/5 ($0.0276, **13/13 initial, no repair needed** —
  the trimmed initial is not starving the smaller tickets), s3 durability
  **FAIL 0/6** ($0.0494, 18 + 12, regressions [1..5]). $0.1405,
  tickets/$ 21.4. Go again. Initial attempt died mid-refactor (mangled
  edit -> syntax errors); the repair reconstructed `GetEx`, made `Persist`
  WAL-durable, fixed two call sites, and hit the cap exactly as its first
  `go build` ran (5 call-site mismatches left: Del/Append/ExpireAt/
  ExpireAfter now return two values). One more build-fix cycle would have
  landed it. **s3 pattern across runs 4/5/7/8: four consecutive failures,
  all "repair ran out mid-fix" on large Go refactors (~36+ turns needed
  vs the 30-turn pool).** Feeding the D6/pool calibration; nemotron cell
  tonight is the second model's evidence.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 7, `..._160300`): reached
  **2** — s0 3/3 ($0.0219, 20t), s1 4/4 ($0.0302, 13t at the reduced cap),
  s2 5/5 ($0.0455, 13 initial + 12 repair — the repair converted the
  pub/sub routing bug), s3 durability **FAIL 0/6** ($0.0441, 18 + 12,
  regressions [1..5]). $0.1417, tickets/$ 21.2, 3 regression-free. s3:
  the initial attempt left `NameError: deque` (client threads died before
  reading — every stage timed out; one smoke test would have caught it,
  none was run); the repair fixed that, discovered writes still failed
  (`main()` never called `wal_recover()`, so `WAL_FD is None`), diagnosed
  it correctly **on its final turn** and the fix edit emitted `def main():`
  twice -> IndentationError -> refused. Third straight cell where the
  repair lands a broken edit at the cap. Genuine; harness clean. Ling's
  effective profile on s3: needs ~35+ turns for the WAL ticket and always
  leaves 1-2 small bugs at the pool edge.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 6, `..._154729`): reached
  **1** — s0 3/3 ($0.0281, 18t), s1 4/4 ($0.0260, 13t — first live use of
  the 12-turn reserve, passed at the reduced initial cap), s2 pub/sub
  **FAIL 0/5** ($0.0368, 13 initial + 12 repair, regressions [1,2,3,4]
  sev 3). $0.0909, tickets/$ 22.0, 2 regression-free. Python again. The
  initial attempt was close (4/5): the pub/sub handlers existed but
  `dispatch` never routed SUBSCRIBE/UNSUBSCRIBE/PUBLISH. The repair round
  diagnosed it correctly, then fumbled the edit tool — `PUT 476*:` was a
  whole-function block replace (it meant the signature line), and a second
  operation line inside the same edit became literal text (`PUT 522>`),
  destroying the dispatch body; the last turns went to fighting the damage
  and the re-verification hit a SyntaxError -> 0/5 -> regressions
  [1,2,3,4]. A genuine "repair made it worse" cell: exactly the cardinal
  sin the regression-free metric is built to catch. Harness clean.
- **Harness validation — eval-only replay** (2026-10-14, `..._151415`, no
  model calls): seeded the run-3 workspace through the full 0-6 spec.
  t0-t3 **PASS** (3/3, 4/4, 5/5, 6/6 — the AOF SIGKILL/restart is clean on
  the fixed binary), t4 **FAIL 6/7**: **stage 8 executed live for the
  first time** — the fault container (8MB tmpfs `/data`, `SDB_DATA_DIR`)
  started, served, and swallowed the verifier's 240KB fill with a hang.
  Reproduced standalone on a workspace copy: the candidate wedges
  permanently on any `SET` >= 64KB (60s+ no reply; 1KB works), i.e., a
  genuine candidate bug caught by the ticket, not a verifier timeout.
  All three advisory probes **PASS** (slow-sender self-seed fix,
  differential vs ref-redis, malformed survival); anchors keep every
  stage network alive. Stage 7 (INFO) and stage 9 (tests&CI) remain
  unexecuted live; canary run 5 in flight chases them.
  *(2026-10-14 follow-up: writing a counting-mock test for stage 7 found
  its `total_connections_received` delta check unpassable — the test
  never opened a second connection, so a faithful server's correctly flat
  counter failed. Fixed: the test now connects/disconnects a second
  client; mock test asserts pass + frozen-counter fail. RULES
  `2026-10-14.7`. Stage 7 still needs a live execution.)*
  *(2026-10-14 follow-up 2: stage 9's revert-sensitivity flow now has an
  end-to-end test against a real fixture git repo — green suite passes,
  toothless always-green suite is rejected. Stage 9 needs a live campaign
  run only if the ladder ever reaches t6.)*
- **Ticket 7 (dashboard) built** (2026-10-14 late, RULES `2026-10-14.8`):
  stage 10 verifier (offline HTML + `/api/stats` deltas + SSE JSON frame
  under concurrent RESP traffic), worker-8090 port publishing gated on
  acceptance id 10, prompt + spec ticket 7. **Validation: verifier logic
  is mock-tested (pass / starved-SSE / frozen-stats); the port extras have
  an args unit test. Live plumbing validation (candidate container with
  the published port + the stage-10 block) still needs a run that reaches
  t7 — no model has passed t3 yet.** Nemotron cell at 2026-10-15 00:05
  runs the 9-ticket spec; its path through t0-t6 is behavior-identical to
  earlier cells (stages 10/11 gate on acceptance only).
- **Ticket 8 (security) built** (2026-10-14 late, RULES `2026-10-14.9`):
  stage 11 verifier — RESP auth gate (`SDB_PASSWORD`), dashboard Basic
  auth (401 bare), oversized-frame probes under a 64MB memory cap
  (64MB bulk header / 2M-arg multibulk must react without buffering), an
  8-payload fuzz corpus with liveness after each, a 12-connection
  slow-loris probe, and a 2000-line container-log scan for the secret.
  Pipeline restarts the candidate with the secret env + `64m` cap and
  restores it afterwards. **Validation: threaded mock test (faithful
  pass / skipped-auth fail / buffered-oversize fail); live plumbing
  awaits a t8-reaching run.**
- **Ticket 9 (perf) built** (2026-10-14 late, RULES `2026-10-14.10`):
  stage 12 — pipelining in order (200 commands in one write, positional
  checks; mock test caught the coalesced-reply reader bug and now covers
  pass + wrong-reply fail), 300-GET sequential p99 budget (10ms), and a
  post-bench gate requiring the stage's own bench measurement to reach
  >= 1.3x the genesis-stage value (carried via `baseline.json`; pure
  `perf_gate_verdict` unit-tested). Measurement study: 5 consecutive
  same-code runs on an idle machine spread 21.6k-22.0k rps (~1%), well
  under the 30% gate; within-campaign stage-to-stage spread for the same
  code was ~5% — the earlier 55k->31k swings were implementation changes
  plus concurrent host builds. **The prompt cannot tell the model its
  exact baseline number (static prompt files); models optimize blind —
  acceptable for v1, noted below.**
- **Open calibration question (for the user)**: ling has failed s3 four
  consecutive times in the "repair ran out mid-refactor" class (runs
  4/5/7/8; needs ~36+ turns vs the 30-turn pool). Options: raise s3's
  pool to 36, raise the repair floor to 16, or keep it as the
  discriminator. Nemotron's cell tonight is the second model's evidence.
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 4, `..._144606`): reached
  **2** — s0 3/3 ($0.0400, 30t), s1 4/4 ($0.0496, 17t), s2 5/5 ($0.0424,
  25t), s3 durability **FAIL 5/6** ($0.0610, 22 initial + 8 repair = 30t).
  $0.1930 total, tickets/$ 15.5, 3 regression-free. **Genuine model
  failure, harness fixes held** (no network murder; the AOF restart
  mechanics worked). This run the model rewrote in Go; the initial session
  never compiled (`main.go` missing `package main`), the 8-turn repair
  fixed both compile errors (package + duplicate `store.del`) and hit the
  limit before testing. Root cause of the AOF loss (reproduced manually on
  a workspace copy): `delFields` uses `make([]byte, 4, ...)` — slice
  LENGTH 4, not cap — so every DEL record carries 4 junk zero bytes before
  the count; replay reads `count=0`, trips the leftover-bytes check, and
  truncates every record after the first DEL (the probe). Classic Go
  slice-length bug; exactly what ticket 3 is for. Advisory 102 failed on
  the pre-`5cf5a86` binary (self-seed fix landed after launch).
  **Calibration signal #2**: the 8-turn repair floor could not recover a
  500-line implementation that never compiled (ling needed ~2-4 more
  turns to test durability; inkling's probe never edited at all).
- **ling-3.1-flash** (spec 0-6, 2026-10-14 run 5, `..._152002`): reached
  **2** — s0 3/3 ($0.0263, 30t), s1 4/4 ($0.0356, 17t), s2 5/5 ($0.0338,
  17t), s3 durability **FAIL 0/6** ($0.0556, 22 initial + 8 repair,
  regressions [1,2,3,4,5] sev 3). $0.1514 total, tickets/$ 19.8, 3
  regression-free. Go again. The initial attempt ended mid-refactor
  (`walLog` undefined, unused imports, never built); the 8-turn repair
  spent 7 turns surveying a 900-line file and its single edit landed at
  the limit — and duplicated a `"sort"` import, so the re-verification
  did not compile either. Harness clean (refused-connection verdicts are
  the candidate's failure). **Calibration signal #3** — see the D6
  amendment below: floor 8 -> 12.
- **ling-3.1-flash** (spec 0-5, 2026-10-14 run 3, `..._140445`): s0-s3 PASS
  3/3/4/5/6 (30t/17t/25t/22t, $0.0286/$0.0333/$0.0369/$0.0752), then s4
  **invalidated by a harness murder**: while the AOF probe had the candidate
  SIGKILLed (2s window), the engine's 60s periodic `prune_labeled_networks`
  sweep — which cannot see foreground CLI campaigns (its busy check only
  covers engine worker_runs) — saw the 354s-old stage network with zero
  *running* endpoints and removed it; `docker start` then failed with
  "network ... not found", stage 8 was skipped, and the repair re-verification
  added a second false regression (stage 4 INCR :2 from WAL state carryover).
  Rollup says reached 3 / regressions [4,6] but both are harness artifacts.
  Fixes: run anchor container (network never looks empty; see `ensure_run_anchor`)
  + stage-4 DEL-first (RULES 2026-10-14.5). **Rerun in flight** to replace
  this cell; keep the dir as incident evidence, do not quote its numbers.
- **ling-3.1-flash** (spec 0-3, D6b): reached **3** tickets — s0 3/3 (30t),
  s1 4/4 (19t), s2 5/5 (19t), s3 6/6 (23t + 7 repair). 4/4 regression-free,
  $0.1743, tickets/$ 22.9. Repair converted a 3-line compile error.
- **inkling:free** (spec 0-3): reached **1** — s0 3/3 (14t, $0.0112),
  s1 4/4 (19t, $0.0192), s2 FAIL 4/5 ($0.0117, 25t: 19 initial + 6 repair).
  Initial attempt hit its cap mid-pubsub; the 6-turn repair spent all six
  turns reading/surveying and never made an edit. $0.0421 total,
  tickets/$ 47.5. **Calibration signal**: the max/4 repair reserve (min 1)
  can be too small for slower models — consider a floor (e.g. 8 turns) or
  larger stage budgets once more cells confirm; do not tune on n=1.
  *(Resolved later same day: floor raised to 12 — see the D6 amendment.)*
- **inkling:free (probe, floor=8)**: reached **0** — s0 PASS 3/3 (30t,
  $0.0156), s1 expiry FAIL 3/4 (17 initial + 8 repair, $0.0071). The
  repair round again read/surveyed for its entire budget and NEVER edited
  (last actions: `head -c 100 server | xxd` -> missing tool -> cap). The
  binding constraint for this model is repair-round *behavior* (it does
  not switch to editing under budget), not the reserve size: 6 turns
  (run A) and 8 turns (run B) both failed the same way, and run-to-run
  variance is large (s0 took 14 turns in A, 30 in B). Floor was kept at 8
  for this cell *(superseded 2026-10-14: raised to 12 after three cells
  showed repair-starved refactors; see the D6 amendment)*; do NOT tune
  further on n=1; collect more cells. $0.0227, tickets/$ 44.1.
- **nemotron:free**: quota-blocked (`free-models-per-day-high-balance`,
  retry ~10.4h). Recorded as a ticket-0 **FAULT** (1 turn, $0.0002) — the
  throttle classification worked; acceptance counted 3 stages, campaign
  stopped. Two refinements landed from this cell: the repair loop is now
  SKIPPED when there are no stage results (no runnable candidate -> a
  repair round is meaningless; the faulted cell burned one against an
  empty workspace), and the rollup says "stopped: ticket 0 fault" instead
  of "ticket 0 failed". Re-run next day.

## Open decisions (operator)

- [ ] D1. HA failover: automatic election (spec as written) vs manual
  promote command.
- [ ] D2. Partition/split-brain: in v1 or a later incident stage.
- [ ] D3. Curve choice (G3).
- [ ] D4. SPRT vs top-up (G2).
- [ ] D5. Repo CI coverage gate 50 -> 80 (`--fail-under-lines`).
- [x] D6. Stage budgets (2026-10-14): option **(b)** implemented — campaign
  stages reserve `max_turns/4` (min 1 turn) for the repair round; initial
  cap = min(max_turns - reserve, 50). Single-shot behavior unchanged; big
  pools unaffected (the 50-cap binds first). See DECISIONS #12. Canary
  re-run under the policy (watch: does ticket 1 convert with repair room?).

## Rules & gates

- Campaign spec + stage prompts + suites join GRADED_FILES -> new stamp
  per campaign version; freeze specs during a wave.
- Flake bar: new stage test passes 20/20 before entering a campaign;
  injectors idempotent + reversible; `test.sh` deterministic.
- No board mixing (single-shot vs campaign); no retroactive rescoring.
- Engine idle during deploys; migrate-check clean after.

## Bench-noise decisions (2026-10-15.6)

- [x] Egress-proxy clarification in the proxy prompts (spec change; one
  model burned ~5 turns on curl's proxy env var).
- [x] Readiness grace for proxy/http candidates (bounded `GET /` probe up
  to 20s before grading; test pins serving / closed / silent-accept).
- [ ] TLS termination ticket and proxy throughput ticket (stage 5 +
  baseline gate) — deferred waves.
- [ ] CI re-enable — repo side is ready (workflow + 80% coverage gate);
  needs the GitHub Actions toggle.
- [ ] Coverage margin wave — biggest residuals: pipeline.rs (~1,650
  missed), web/server.rs (1,098), web/queue.rs (798), verifier/redis.rs,
  verifier/http.rs (415), report/leaderboard.rs.

## Hygiene backlog

- [x] Incident fix (2026-10-14): a SIGTERM-killed campaign leaked its
  reference container holding host port 6380 for 2h; `start_reference_redis`
  swallowed docker-run failures (`let _ = …; Ok(())`), so every later
  campaign silently measured against a missing ref ("Could not connect to
  ref-redis") with no warning. Fixed: both reference starters return Err
  with stderr on failure, and the campaign CLI wires SIGINT/SIGTERM to the
  pipeline cancel flag so cleanup guards reap running containers. After a
  hard kill, `prune --yes` remains the belt.
- [x] Incident fix (2026-10-14, engine-sweep murder): the engine's 60s
  periodic reconcile runs `prune_labeled_networks` while a foreground CLI
  campaign is live (the busy check only sees engine-owned worker runs), and
  a stopped candidate counts as zero endpoints — so a >300s-old stage net
  whose candidate is in its AOF kill window was removed mid-stage (s4
  "network ... not found"). Structural fix: every run network now carries a
  `subdollar-anchor-*` container (`ensure_run_anchor`) that keeps the
  network non-empty for the run's whole intended life; zombie anchors are
  reaped by the orphan sweep's 3h age gate. Side effect: fixes the
  equivalent race for any future sweeper, ours or not.
- [x] Cleanup (2026-10-14): pre-purge recovery branch + `refs/original`
  dropped, stale stash dropped, reflog expired, gc pruned the leaked-key
  blobs (verified absent). Stale root logs removed; data archived + reset
  (see TODO_EFFORT §S).
- [ ] README: campaigns section once Slice A lands.
- [ ] DECISIONS.md: one entry per resolved open decision.

## Scope guards

- Do not start campaign #2 before sdb-kv canary + one grid pass.
- No new task types until the campaign machinery proves.
- No curve/scoring changes without an anchored wave.
