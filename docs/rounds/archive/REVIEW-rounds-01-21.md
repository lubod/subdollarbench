# Haiku 5.5 career on `.36` (2026-10-21, after the round-21 merge)

Reviewer: Claude Code. Model `openrouter/anthropic/claude-haiku-5.5`,
effort high, all 4 phases chained, rules `2026-10-19.36` (master
`020040a`). Every phase stayed under the $0.99 / 150 min cap.
No harness fault in any phase.

| Phase | Cell | Points | Cost |
|---|---|---|---|
| P1 0–6 | `…_20261021_145301` | 8.8 / 10 | $0.4886 |
| P2 7–10 | `…_20261021_163524` | 7.2 / 12 | $0.7050 |
| P3 11–15 | `…_20261021_184431` | 8.0 / 22 | $0.7076 |
| P4 16–20 | `…_20261021_195345` | 15.8 / 32 (+1.6 catch-up headroom) | $0.7566 |
| **Career** | | **39.8 / 76** | **$2.6578** |

Final `ladder_state`: passed 0–9, 12, 13, 17, 19 (lite), 20; clean,
last_green `30937eb`.

- **P3:**
  - 11 failed its late perf gate: "no such column: k" on a fresh scratch
    store, a candidate bug.
  - 12 and 13 passed.
  - 14 was skipped (it needs 11).
  - 15 failed both full and lite: the candidate's own `./test.sh` had 3
    failing tests.
- **P4:**
  - The 11-lite catch-up failed: the server didn't start (SyntaxError).
    The workspace was rolled back.
  - 16 was skipped (it needs 11).
  - 17, 20 and 19-lite passed.
  - 18 failed; it has no lite.
- **Rollback/carry-over worked as designed:**
  - P3 and P4 each reset to the carried last_green.
  - The catch-up ceiling was added to the P4 max (Q9).

## Findings for round 22

- **F1, unhelpful feedback (ticket 18):** the security scenario failed
  twice with `read: Resource temporarily unavailable (os error 11)`.
  - That is the raw `EAGAIN` from a socket read timeout
    (`src/verifier/sql/wire.rs` `read_line`; the same pattern is in
    `pgwire.rs:198`).
  - The candidate got the identical opaque line both rounds and could not
    act on it.
  - It should read e.g. "no answer within Ns to `<statement>`" and name
    the scenario step.
  - Message-only. Correction: `wire.rs` is *not* in `GRADED_FILES`;
    nothing of sdb-sql is (see F4 in the TODO).
- **F2, a crashed session is graded as a fault and loses its lite
  (grading):** P2's ticket 10 and P3's ticket 11 both failed the same way.
  - Haiku's turn hit the 65,536-token output limit, which truncated an
    `edit` call (invalid JSON). The next request got `400 Provider
    returned error`, and OMP exited with status 1.
  - That set `agent_failed`, so the status was `fault`. The `else if
    fault` branch skips the lite, although the verifier had graded the
    round (11: 10/11) and there was no `harness_fault`.
  - Ticket 11 has a lite and never got it. 14 and 16 were then skipped,
    and P4 spent its catch-up on 11-lite.
  - Fix: a graded round with no `harness_fault` is `failed`, and the lite
    runs. Also check whether OMP can turn a truncated tool call into a
    recoverable tool error.
- **F3, minor (carried over from round 21):** while a cell runs, a ticket
  whose need hasn't run yet shows `skipped` (14 during P3) instead of
  `waiting`.

# Review: round-21 fixes (2026-10-21, round 21)

Reviewer: Claude Code. Scope: `68aee9c` "Round-21 memory-proof, S1 and S2
fixes" (rules `2026-10-19.37`, branch `round21`, not merged). Nothing was
committed, no service was restarted and no cell was launched.

**Verdict: correct. Nothing in `.37` changes grading.**

- **Memory-cap proofs:** the 8 failures were a test-setup bug, not a
  production problem.
  - The test scratch dir lived under `/tmp`, which is tmpfs on this host.
    Files a capped candidate wrote there were charged to its cgroup as
    unreclaimable shmem.
  - I confirmed `/tmp` = tmpfs, while `runs/` and `/var/tmp` = ext4. So
    production cells (data under `runs/<cell>/`) were never affected.
  - The fix (`covtest_dir()` -> `/var/tmp`, with `SDB_COVTEST_ROOT` as an
    override) is test-only. It touches no cap and no candidate launch.
- **S1:** fixed as asked.
  - `groupCells` keeps faulted cells.
  - A faulted phase cell renders as `faulted (non-result)` under its
    career, and `careerLeaf`/`careerPoints` still ignore it.
  - A faulted full-ladder cell is a normal row again.
  - The smoke fixture covers both cases.
- **S2:** fixed as asked.
  - `refork.of` marks the superseded branch as used.
  - The newest successor by `started_at` wins.
  - Tests cover the `P1 -> old / retry` case.

## The rules bump is the only decision

- **What `.37` touches:** test scaffolding, the Board and the
  chain-launch policy.
- **What it doesn't touch:** none of these files are in `GRADED_FILES`, and
  scores can't move.
- **Why it matters:** `validate_chain` requires equal `rules_version`. With
  `.37`, the Haiku career on `.36` (phase 1: 8.8/10 for $0.49; phase 2:
  7.2/12 for $0.71) can never continue.
- **Phase 4 is no longer blocked on `.36` either:** the memory "blocker"
  was the test setup.

## S3: the GUI phase view (done by the reviewer, uncommitted)

The operator asked for it twice. The work is in the scratch worktree
`gui-phases` (on `c960462`) and is not committed.

- **Strip:** four labelled phase groups. Earlier phases are `carried`
  (dimmed, in the carried outcome's colour); later phases are
  `later_phase` (outline).
- **Career row:** `P1 8.8/10 · $0.49 | P2 running · 3.0/12 · $0.07 |
  P3 —/22 | P4 —/32`, and a running phase's state on the career row.
- **Phase-aware ladder:** `campaign::phase_ladder` counts the carried
  passes towards `needs`.
- **Live key:** the career phase number moves to `live.career_phase`, so
  it no longer clashes with the activity key `live.phase`.
- **API:** the Board and Run responses carry a `phases` table.
- **Checks:** checked against the real Haiku cells with headless-Chromium
  screenshots at desktop and phone width. There is a Rust test
  (`s3_phase_ladder_carries_earlier_and_marks_later_phases`) and smoke
  checks; I proved the smoke checks run by breaking one on purpose.
- **Merging with `round21`:** the patch applies on top of `round21` with
  one smoke-test conflict. Both sides are kept. fmt and clippy are clean.

---

# Round 20 (closed)

# Review: round-20 pre-cell fixes (2026-10-21, round 20)

Reviewer: Claude Code. Scope: `1b9a0b5` "Round-20 pre-cell fixes" (rules
`2026-10-19.36`). Nothing was committed, no service was restarted and no
cell was launched. The stashes were not touched.

**Verdict: `.36` does what the round-20 TODO asked. Nothing blocks the
phase-1 cell.** There are two LOW findings, S1 and S2, plus one planning
point (P below) that changes the order of the next steps.

**Tests (niced, master tree, no cell running):**

- `cargo test --lib -- campaign:: main_campaign:: web::server::tests`:
  122 passed, 1 ignored. This includes the JS smoke test with the new
  career fixture.
- `cargo test --bins`: 63 passed.
- `cargo fmt --check` and `cargo clippy --all-targets`: clean.
- I did not rerun the full lib suite. The agent reports 985 passed, with
  the 8 memory-cap live proofs filtered out.

## Fix check

| Item | Status | Where |
|---|---|---|
| R1/D2: any started successor uses up the chain | fixed | `campaign::chain_already_used` reads the rollup, then status `chain_from`, then `live.chain_from`. It skips a cell only on `harness_fault`. Finished, cancelled, crashed and running cells all count. |
| R1: `chain_from` in status.json at creation | fixed | `main_campaign.rs` `update_status` writes it before the workspace copy. `update_status` merges, so a later `write_status(.., "cancelled", None, None)` keeps it. |
| R1: `--refork "<reason>"` | fixed | `resolve_refork` refuses: a used chain without `--refork`, `--refork` without `--chain`, an empty reason, and `--refork` on an unused chain. It records `{of, reason}` in the live block and the rollup. The CLI checks under the campaign lock; the server pre-check returns `409` with `refork_required`/`chain_used_by`. |
| R1: console and Board | fixed | A reason field appears after a 409, and the POST carries `refork`. The Board row shows a `refork:` badge. `sql-cell.sh` passes `"$@"` through, so `--refork` works there too. |
| R1: tests | fixed | Covers cancelled, status-only, running and faulted successors, and every `resolve_refork` branch. |
| R2: career leaf | fixed | `careerLeaf` = the finished, non-faulted cell no finished cell names as `chain_from`, newest `started_at` first. Points and cost come from the leaf. The older sibling is labelled "superseded". A running cell follows `chain_from` to the root (`careerRoot` + `live_rollup_stub` now fills `phase`/`chain_from`). |
| R3 | fixed | The filter is `passed`, and each section is labelled full/lite. |
| D1 | done | The rules history says "operator decision 2026-10-21". |

## S1. LOW-MEDIUM: the Board now hides every harness-faulted cell, not only faulted phase cells

`index.html` `groupCells`: `if (!r.model || r.harness_fault) return;` runs
before the phase check.

- **Full-ladder cells:** a faulted full-ladder cell was a Board row with
  stop reason "harness fault". Now it is gone. R2 asked to drop faulted
  cells from *careers* only.
- **Phase cells:** a faulted phase cell disappears entirely as well. That
  is the row the operator opens to see why it faulted and to decide a
  rerun.

**Fix:** keep faulted cells in their group. `careerLeaf` already ignores
them for points and the leaf, so they only need to render as an indented
`faulted (non-result)` row under the career, and as before for full-ladder
cells. Add both cases to the smoke fixture.

## S2. LOW: a superseded branch can be continued without `--refork`

After `P1 -> old(phase 2)` and `P1 -> retry(phase 2, refork of old)`,
`--chain old --phase 3` is accepted. Nothing names `old` as `chain_from`,
and `validate_chain` doesn't look for refork records.

On the Board, the leaves are then `retry` and `old -> phase 3`. The newest
`started_at` wins, so the career silently goes back to the abandoned
branch, with no badge on the top row. The `superseded` label also only
compares siblings of the top row, so neither `old` nor `retry` gets
labelled.

**Fix:**

- **CLI:** in `chain_already_used`, or next to it, treat a chain cell named
  in any non-faulted cell's `refork.of` as used. Continuing it then needs
  `--refork` too.
- **Record:** fill `refork.of` with the newest existing successor by
  `started_at`. Today it is whichever entry `read_dir` returns first, which
  is arbitrary once there are two.
- **Test:** the CLI scan for this case.

## S3. MEDIUM (P1, missed in round 19): the GUI has no phase view

The operator found this on the first live phase cell
(`…claude-haiku-5.5_20261021_145301`): its Board row is one 0–20 strip.

- **S3a. The Board career row was never built.** P1 item 5 asked for
  "one row per model career, with per-phase points and cost columns and
  the career total". What exists is a group with a cumulative top row and
  expandable phase rows, each with the full 21-ticket strip. I missed this
  in round 19.
- **S3b. The strip ignores phases.** `campaign::ladder` walks every stage.
  So:
  - a phase-1 cell shows tickets 7–20 as "not reached" or "skipped";
  - a phase-2 cell shows the carried-over 0–6 as unpassed, because its
    rollup only holds its own stages.
- **S3c. A running phase cell loses its phase.**
  - The live updater uses `live.phase` for the activity
    (`"agent"`/`"verifying"`, `main_campaign.rs:218-284`). That overwrites
    the career phase number written at creation (`:522`).
  - So `live_rollup_stub` gets `phase: None`, the cell groups as a
    full-ladder cell, and its row reads `0/76` instead of `0/10`.
  - On the live API right now: `phase None, max 76`, and tickets 1–20 are
    all "skipped".

None of this touches grading. It is display only.

## P. Planning point: a career must run on one rules version

`validate_chain` requires `prev.rules_version == RULES_VERSION`
(`campaign.rs:1001`).

- **What breaks:** phase 4 needs the memory-cap fix. If that fix bumps the
  rules, and a verifier-side change should, then a career started on `.36`
  can never reach phase 4. The same goes for S1/S2 if they land as `.37`
  between phases.
- **What follows:** a career that is meant to reach phase 4 has to start
  after the memory-cap fix lands.

## Next steps

1. **Operator: run a shakedown career on `.36` now.** This is the
   recommendation; each step needs a go-ahead.
   - Rebuild and restart on `1b9a0b5`.
   - Run phase 1: `SDB_CELL_PHASE=1 scripts/sql-cell.sh <model>`.
   - Run phase 2, then phase 3, each with
     `SDB_CELL_CHAIN=<previous cell id>`.
   - Cost: about $3 in all.
   - What it shows: carry-over, chaining, the first live catch-up and
     hints, and the Board career row on real data.
   - Limit: it is a test career and can't reach phase 4.
   - The alternative is to wait for `.37` and run only the real career.
2. **Other agent, in parallel, in its own worktree.** Don't touch master's
   tree or `target/release` while a cell runs.
   - The memory-cap investigation (kernel 7.0.0-38).
   - S1 and S2.
   - One commit, rules `.37`.
3. **Reviewer:** review `.37`.
4. **Operator:** run the real career, phases 1 to 4, on `.37`.

---

# Round 19 (closed)

# Review: career phases P0-P2 and fixes (2026-10-21, round 19)

Reviewer: Claude Code. Scope: the three commits after `6d270b8` (the
TODO):

- `8beea56`: P0 cap enforcement (rules `.32`)
- `166db6b`: P1 phases and chaining (rules `.33`)
- `ba3ab6e`: P2 catch-up slots and hints (rules `.34`)

The stashes were not touched and nothing was committed. I ran the probes
in a scratch worktree at `ba3ab6e` and removed it afterwards. No service
was restarted and no cell was launched.

**Verdict:**

- **P0 is correct and complete against the spec.** No findings above LOW.
- **P1 is correct as plumbing:** phases, the chain check, the carry-over
  and `ladder_state.json`. Two MEDIUMs are about how careers are *ranked*
  and *forked* (Q5, Q6).
- **P2 has one HIGH and three MEDIUMs in the catch-up slot (Q1-Q4).** I
  confirmed three of them with probe tests. Fix these before the first
  career is published: the catch-up as built can't rescue the most common
  phase-1 failure (ticket 4).
- **Tests (niced, run by the reviewer):**
  - `cargo test --lib campaign::`: 50 passed.
  - Both harnesses' fake-docker scenarios: 3 passed, including the new
    `cap` scenarios.
  - `cargo clippy --all-targets`: clean.
  - I did not rerun the full suite. The agent reports 8 memory-cap live
    proofs failing on this host (see "Environment" below).
- **Ops:**
  - The services and `target/release` are still `9f2ff24` (rules `.31`).
    The engine restarted at boot, 06:59:59.
  - None of P0-P2 is live: the console has no phase picker yet, and
    `sql-cell.sh` would run `.31`.

## Fix check: `180be0f` "Round-19 career-phase fixes" (rules `.35`)

**Verdict: Q1-Q9 and the nits are fixed.** Two new LOW-MEDIUMs (R1, R2)
and one nit (R3).

**Tests (niced, run by the reviewer):**

- `campaign::`, `portal` and the fake-docker scenarios: 94 passed.
- Bins: 62 passed.
- clippy and fmt: clean.
- The new tests cover my three probes:
  - `q1_a_skipped_4_5_6_chain_uses_three_slots`: the order is 4 → 5 → 6,
    and then 12 and 14 run and pass;
  - `q3_a_catch_up_harness_fault_stops_the_phase`: only 4 runs, and the
    fault reads `ticket 4: …`;
  - `catch_up_targets…`: 11 is not eligible while 4 is failed (Q2).

| Item | Fix | Check |
|---|---|---|
| Q1 | Greedy over `Failed` or `Skipped` tickets, recomputed after each slot. There are three slots (`CATCH_UP_SLOTS`), and a ticket is not retried within the phase. | OK. The third slot was the agent's choice; I had asked the operator to decide (see the next steps). |
| Q2 | A target's `needs` must be in `passed_all`. | OK. |
| Q3 | `stop_on_harness_fault`, a state snapshot, and the main loop is skipped. | OK. |
| Q4 | `render_spec_md_catch_up` (every passed ticket, the target last) and `catch_up_accepted` (declared ∪ passed). Both are used when `job.catch_up` is set. | OK. |
| Q5 | `BoardSortKey` uses the `career` totals. The console groups by career root (`career.cells[0]`) and shows the leaf's cumulative total over 76. Full-ladder cells are their own class. The portal skips any rollup with `phase`. | OK, apart from R2. |
| Q6 | `chain_already_used` scans `runs/` under the global campaign lock, now taken before the chain check. | Race-free, apart from R1. |
| Q7 | `LadderState.last_failures` is filled from every record in `on_stage` and at the end. Hints fall back to it. | OK. Entries for tickets that later passed stay in the map, which is harmless because hints only read broken tickets. |
| Q8 | `max_effective_usd` and the repair cap are net of a $0.002 final-verification reserve. | OK. |
| Q9 | The ceiling grows by 0.4 × tier for each catch-up *attempted*. | OK. |

### R1. LOW-MEDIUM: cancelling a successor frees the chain again

`chain_already_used` skips successors with an empty `finished_at`. A
cancel (`write_status("cancelled", None, None)`) leaves the last
`on_stage` rollup, whose `finished_at` is empty, and so does a crash.

So an operator can cancel a phase-2 cell that is going badly and re-run
phase 2 from the same phase-1 cell. That is best-of-k through cancel, the
case Q6 was meant to close.

**Fix:**

- Count any successor that *started* (has `chain_from`), unless it is
  harness-faulted.
- Allow an explicit `--refork <reason>` for crashes and reboots, recorded
  in the new rollup (`refork_of`, reason) and shown on the career row.

The test also doesn't isolate the faulted case. It asserts `is_some()`
while the finished child still exists, so it would pass even if faulted
cells claimed the chain.

### R2. LOW: the Board's career leaf is chosen by phase only

`careerPoints`, `careerCost` and the row sort pick the highest `phase`.
With a faulted or cancelled phase-2 cell plus its retry, which R1 and
harness faults allow, both share the root and phase 2. The leaf is then
whichever sorts first, which can be the faulted one.

A live phase cell also has no `career` block until it ends. Mid-run it
shows as its own one-cell group.

**Fix:**

- Take as the leaf the finished, non-faulted cell that no other cell in
  the group names as `chain_from`.
- Ignore faulted cells when grouping.

### R3. Nit

`render_spec_md_catch_up` ignores `outcomes` for tickets in `passed`
that are not `Full` or `Lite`. That can't happen today, but the
parameter is redundant with `passed`.

### Still open: the memory-cap live proofs

The memory-cap proofs are still open on kernel `7.0.0-38` (cgroup v2,
systemd driver). The fix commit records them as an open environment
item. The rules are now `.35`, so the 8 skipped proofs are the only
gates not green.

## What I checked and found correct

- **P0 repair guard** (`pipeline.rs`):
  - It runs before the pool check, the candidate stop and
    FEEDBACK_REPORT.md.
  - A repair gets `budget - metered`, plus an effective ceiling of
    `headroom - (api + floor so far)`. `floor_so_far` is timed from the
    start of the attempt, so it includes round 0's verification.
  - Replays: $0.1307/$0.12 (Haiku t9) and $0.0364/$0.02 (Haiku t10) both
    refuse the repair.
- **P0 in-session stop:**
  - OMP checks in its 50 ms status loop, opencode in its 100 ms wait
    loop.
  - Token accounting matches `compute_cost`: fresh `input`/`output` plus
    `cacheRead` in OMP, and the same totals in opencode. The live log
    confirms `input` excludes cache reads (Haiku round tokens
    `prompt=40`).
  - A trip returns `Ok` with the live-metered `cost_usd` and
    `max_call_usd`, and never fires the API-budget line (the `cap`
    scenarios).
- **P0 cell reserve:**
  - A ticket or lite attempt starts only when
    `cap - total >= max(largest call, 0.02) + 0.02`.
  - `api_ratio` is applied to the net headroom.
  - The lite check uses the reserve the full attempt has just raised.
- **P1 chain check:** the chain cell must be the previous phase, with the
  same model, harness, effort and rules version. It must be finished, not
  harness-faulted, and carry a ladder state. A chain id with `/` or `..`
  is refused, in the CLI and in `POST /api/campaigns`.
- **P1 carry-over:**
  - Copied: the workspace including `.git`, the store and snapshots,
    sentinels and `baseline.json`.
  - `last_green` comes from the live `Arc` in `main_campaign`'s
    `on_stage`, not from the stale chain value that `run_campaign_ctx`
    snapshots.

## Q1. HIGH (P2): a skipped ticket is never a catch-up target, so a phase-1 ticket-4 failure still blocks all of phase 4

`catch_up_targets` only considers `LadderOutcome::Failed`. The needs
graph makes this the common case:

- If ticket 4 fails in phase 1, then 5 and 6 are **skipped**.
- Every phase-4 ticket needs 5 or 6: 16→6, 17→5, 18→6, 19→6, 20→6.
- In phase 3, 12 needs 6, and 14 needs 6 and 11.

**Probe** (phase 3, state with 4 failed and 5/6 skipped, every attempt
passes):

- the run order is `[4 (catch-up), 11, 13, 15]`;
- 12 and 14 stay **skipped**, and 13.8 of 22 points are earned;
- the second slot is unused, although 5 became runnable the moment 4
  passed.

The career can't reach 12, 14 or any of phase 4's 32 points, even though
the catch-up worked.

**Fix:**

- Choose slots greedily, one at a time. The candidates are any `Failed`
  *or* `Skipped` earlier ticket whose own `needs` are now in
  `passed_all`, ranked by the phase points it unblocks.
- Recompute after each slot, so the second slot can take 5 after 4
  passes.
- Even then, the 4→5→6 chain needs **three** slots. Decide whether that
  case gets a third slot or stays a lost career (an operator decision).
  Record it in the rules history either way.

## Q2. MEDIUM (P2): a catch-up ignores its own `needs`

The catch-up loop never checks `stage.needs ⊆ passed_all`.

**Probe** (phase 3, 4 and 5 failed, 4's catch-up fails): the run order is
`[4, 5, 12, 15]`. Ticket 5 runs, and is paid for, although it needs 4,
which just failed. The main loop would have skipped it. If 5 then
passes, it is recorded as passed on top of a failed prerequisite, which
breaks the needs invariant the scoring rests on.

**Fix:** skip a target whose needs are unmet. Q1's greedy selection
covers this.

## Q3. MEDIUM (P2): a harness fault in a catch-up does not stop the cell

On `out.harness_fault` the catch-up loop sets `rollup.harness_fault` and
`break`s, but only out of the catch-up loop. The main loop then runs the
phase anyway.

**Probe:** the catch-up of 4 returns "never reached first turn". The cell
then runs `12 full, 12 lite, 15 full, 15 lite`: four more paid attempts
in a cell that is already a non-result.

The reason is also stored without the `ticket N:` prefix that
`stop_on_harness_fault` adds.

**Fix:** call `stop_on_harness_fault`, snapshot the state, and return
without entering the main loop.

## Q4. MEDIUM (P2): a catch-up's SPEC.md and acceptance cover only tickets before it

- `render_spec_md` lists `s.id < stage.id`.
- The acceptance is the target's own list (ticket 4: `sql_stages
  [0..4]`).

Take a catch-up of ticket 4 at the start of phase 3. The model doesn't
see the contracts of 5-10, which passed in phases 1-2, and nobody checks
them. If the catch-up breaks joins or aggregates, the failure appears at
ticket 11's acceptance (`0..11`): 11 is sunk as a regression, and the
cause is two attempts back.

**Fix:**

- For a catch-up, render every passed ticket regardless of id, with the
  target's section last.
- Grade the declared list plus `passed_all`.
- Add a test: a catch-up at 4 with 7-10 passed must verify 7-10.

## Q5. MEDIUM (P1): the Board and the portal don't rank careers

- **Grouping:** the Board groups by model, harness, effort and rules.
  All phase cells of a career, and any full-ladder cell on the same
  rules, share one group.
- **Ranking:** `board_order` ranks on a single cell's points. A phase-4
  cell (max 32) outranks a better career whose best cell is phase 1
  (max 10).
- **Median:** the group median mixes phases.
- **Career hint:** it takes the highest-phase cell that has a career
  block. That is not necessarily the chain the top row belongs to.
- **Portal:** the export has no phase or career awareness.

**Fix:**

- Make a career row from the chain: the root to its leaf, via
  `chain_from`.
- Rank it on `career.points`, with the per-dollar ratio from
  `career.total_cost_usd`.
- Show its phase cells as the expanded rows.
- Give full-ladder cells their own row class.
- Until then, do not publish phase cells to the portal.

## Q6. MEDIUM (P1): a phase cell can be continued more than once (best-of-k careers)

Nothing stops two phase-2 cells chaining from the same phase-1 cell, or
re-running phase 3 until it scores well. A career then becomes the best
of k tries per phase, and the Board can't tell.

**Fix:**

- Refuse a chain cell that another finished, non-faulted cell already
  names in `chain_from`. Do the check under the campaign lock, by
  scanning `runs/` or by writing a `chained_by` marker.
- Or allow forks, but show each fork as its own career and count the
  tries.

## Q7. LOW (P2): hints only read the immediate predecessor's records

`phase_hints` looks the failure up in `prev.stages`. Suppose ticket 4
failed in phase 1 and was not caught up in phases 2 or 3. By phase 4 its
hint says "(no recorded failure line)". Carry each broken ticket's last
`FailureSummary` in `LadderState`.

## Q8. LOW (P0): the in-session cap does not reserve the last verification's floor

The stop is at `cap - total - reserve`. After it, the final
verification's compute floor is added outside any limit.

On Haiku tickets 8-10, verification took 15-25 s, about $0.0006. The
worst case is `cap + verification floor`, inside the $0.01 margin under
$1.

Check the `[COST]` line on the first cell that reaches ticket 16, whose
memory proofs take longer.

## Q9. LOW (P2): catch-up points can exceed the phase's `max_points`

Phase 3 with every ticket passed, plus a ticket-4 catch-up, scores
22.8 / 22. The career total still can't exceed 76.

Either add the catch-up tiers × 0.4 to the phase ceiling shown, or label
the ceiling "excluding catch-ups".

## Nits

- **Commit messages:** the shell expanded `$0` in two of them.
  - `8beea56` says "the /bin/bash.02 budget/cap floor".
  - `166db6b` says "skipped at /bin/bash".
  - They are on master already, so leave them.
- **`src/main_campaign.rs:1597`:** the doc comment is duplicated in one
  line ("`run` fails on a missing campaign name in load_campaign ///
  `run` fails…").
- **Stray attribute:** `#[allow(clippy::too_many_arguments)]` sits above
  `with_hints`, which takes two arguments.
- **Misplaced test:** `launcher_args_carry_the_phase_and_chain` was
  inserted under the doc comment of `shipped_sdb_sql_campaign_continues`.
  That doc now describes the wrong test.
- **Hint text:**
  - It says "SPEC.md section: Ticket 8 - Joins", but the heading is
    "# Ticket 8 — Joins".
  - It mentions "the catch-up slots below", but nothing follows.
- **Dead code:** `campaign::cap_headroom()` is only used by a test.

## Environment (reported by the agent, not rerun by me)

Eight memory-cap live proofs fail on this host, and identically at
`9f2ff24`: SQL T16, the T17-20 cascade and KV stage 14. The reference is
OOM-killed (exit 137) after about 100k rows under the 64 MiB cgroup.

This started after the reboot onto kernel `7.0.0-38`. Ticket 16
("Larger than memory") grades against that same proof, so **phase 4 is
not runnable as graded until this is understood**. Phases 1-3 are
unaffected.

## Next steps (operator decisions 2026-10-21)

- **Decided:**
  - **Catch-up slots:** three.
  - **Successors:** a started successor uses up its predecessor; a
    rerun needs `--refork "<reason>"`.
  - **First cell:** no real phase cell until `.36` is reviewed.
- **The other agent's plan:** `TODO_CAMPAIGNS.md` § "Round-20 work". It
  covers R1 (`--refork`), R2 (the career leaf), R3 and the rules bump to
  `.36`, plus the memory-cap investigation as a separate task.
- **Then:**
  - the reviewer reviews `.36`;
  - with the operator's go-ahead, rebuild and restart;
  - then phase 1, phase 2 and phase 3;
  - phase 4 only after the memory proofs are green.

---

# Round 18 (closed)

# Review: round-17 fixes (2026-10-20, round 18)

Reviewer: Claude Code. Scope: `8f4338d` "Round-17 review fixes (rules
`2026-10-19.29`)", the only commit after `7f78977`.

The stashes were not touched. All experiments ran in scratch worktrees at
`8f4338d`. On the operator's go-ahead, the `.30` fixes (below) were
committed as `7bb4955`, merged to master, and are live.

**Verdict:**

- **Every round-17 finding is fixed: J1, J2, J3 and K1-K3.** Each fix
  follows the round-17 design, and I confirmed each one live or in a mock.
  - **J1:** on the live reference cluster, the new freeze test passes the
    unchanged reference **5 of 5** times (full) and **5 of 5** (lite). It
    catches the reference patched to ack before any replica has the
    commit **12 of 12** times (full and lite, with 50 ms and 300 ms replica
    polling).
  - **J2:** a replica that looks the table up before it checks its role
    now passes, full and lite.
  - **J3:** a reverted server that takes 8 s to start and 2 s to answer
    now passes the boot probe. Before, 250 ms was too slow.
- **No HIGH or MEDIUM findings.** There are four LOWs (N1-N4) and one nit
  (N5).
  - N1 is new in `.29`: the lite scenario samples the replica's LSN once,
    right after the ack.
  - N4 is older (ticket 12, since `.18`). The other agent's own gate run
    hit it.
- **Ops:**
  - The other agent rebuilt and restarted the services on `.29` at
    15:03:18, 46 s after the commit.
  - Nothing here blocked the capped cell. On the operator's go-ahead it
    ran on `.29` from 15:42:06 to 16:52:54. It reached ticket 19 with
    34.6 of 76 points and $0.748 spent (the cap), and had no faults. See
    "Calibration cell".
  - On the operator's request, I prepared `.30`: N1-N5, plus N6, which
    the cell exposed. Its full suite is green. With the operator's
    go-ahead it is committed (`7bb4955`), merged to master and live since
    17:43:52. See "`.30`".

## Test results (HEAD `8f4338d`, reviewer worktree, niced)

- `cargo fmt --check`: clean. `cargo clippy --all-targets -D warnings`:
  clean.
- `cargo test` (default parallelism, 15:21-15:55):
  - lib **971 passed, 0 failed**, 6 ignored, 1979 s;
  - bins 60 passed; other targets 2 passed.
  - Every live proof passes: 13-18, 19 full and lite, 20 full and lite.
    So do both ticket-15 sensitivity end-to-end tests.
- Probes:
  `/tmp/claude-1000/-home-ubuntu-subdollarbench/b1b03a05-74a3-453d-b2d7-842ae849a2b9/scratchpad/round18-probes.patch`.
  - It adds an `r18_probes` module to `replication.rs` with two
    `#[ignore]` tests:
    - `r18_live_freeze`: the committed `scenario` and `lite_scenario`
      against a Docker reference cluster on the worker-41 ports, with the
      pipeline's control mapping, timing each action;
    - `r18_mock_apply_lag`: a mock sync engine that acks on receipt and
      applies after a set lag (N1).
  - It also adds `r18_snapshot_counts` to the tests module of
    `concurrency.rs` (N4). To make the count visible, the probe raises
    `MIN_SNAPSHOTS` in the scratch copy only.
  - J3 was checked with the probe's own Python, extracted from
    `pipeline.rs`, against a dummy server.

## Round-17 status

| Item | Status | Notes |
|---|---|---|
| J1 | **Fixed** | Freeze, `COMMIT` with a 1 s deadline, resume, equal LSNs, kill. Live results below. The docs, the module comment and the `.28` history line are corrected. |
| J2 | **Fixed** | The refusal runs after both replicas reach the seed LSN. The new `TableFirst` mock passes, full and lite. |
| J3 | **Fixed** | 25 s to start, 10 s to answer, 45 s cap. Worst case 35 s, inside the cap. |
| K1 | **Fixed** | Lengths are bounded to 4..=64 MiB. A post-password `AuthenticationOk` reads "accepted the wrong password". |
| K2 | **Fixed** | Five negative tests: async ack, table-first (full and lite), any-password PG, no-ticket-9 path. There is no lite async-ack mock test, but both scenarios share `seed_commit`, and the live probe covers the lite. |
| K3 | **Fixed** | Comments restored. `suite_variants` is extracted and tested; one inline copy remains (N5). The skip message names the failed stage. The README pulls `postgres:16-alpine`. |

## J1 evidence: the freeze test on the live reference

`r18_live_freeze` runs the committed scenarios against the reference in
Docker. Pause, resume, kill and rejoin map to the same `SandboxManager`
calls as the pipeline.

| Reference variant | 3 nodes | 2 nodes (lite) |
|---|---|---|
| unchanged: `COMMIT` waits for a replica's ack | **5/5 pass** | **5/5 pass** |
| acks before any replica has the commit; replicas pull every 50 ms | caught 3/3 | caught 3/3 |
| same, replicas pull every 300 ms | caught 3/3 | caught 3/3 |

- **Every caught run** failed with "COMMIT answered within one second while
  every replica was frozen: Ok(0)".
- **The freeze lasts 1.05-1.18 s** in the full scenario and 1.01-1.11 s in
  the lite. Each figure spans from the end of `docker pause` to the start
  of `docker unpause`.
- **The election is deterministic now.** All five full runs elected node 2,
  the lower port, at LSN 18.
- **The failover is slower but well within budget.** The rejoin started
  3.7 s after the kill, because the reference now waits 3 s before it
  elects.
- **The failure path cleans up.** Every caught run returned with the
  replicas still paused. The pipeline's cleanup still worked:
  - `docker logs` read each node;
  - `stop_ha_nodes` removed the paused containers in 0.4-0.8 s;
  - no containers were left (Docker 29, cgroup v2).
- **The round-17 race is gone.** Both replicas reach the commit's LSN
  before the kill, so the post-election catch-up can no longer change
  the expected winner.

## J3 evidence

| Dummy server | Probe result |
|---|---|
| starts at once, answers after 0.25 s | exit 0 after 0.3 s (failed in `.28`) |
| starts after 8 s, answers after 2 s | exit 0 after 10.1 s |
| starts at once, answers after 12 s | exit 1 after 10.1 s (`TimeoutError`) |
| never starts | exit 1 after 25.2 s ("server did not start within 25 s") |

## N1. LOW (ticket 19 lite, new in `.29`): the replica's LSN is sampled once, right after the ack

- **Where:** `lite_scenario` (`replication.rs:445`), immediately after
  `seed_commit` returns. One `SHOW ROLE` on the replica must already show
  `lsn >= commit_lsn`.
  Otherwise the scenario fails with "the promoted replica is at lsn …:
  the ack was not synchronous".
- **What changed:** `.28` checked this after `PROMOTE` and `wait_role`.
  `.29` moved it before the kill, as a single sample.
- **Effect:**
  - The contract says the replica must have the commit "durably" before
    the ack. It doesn't define a replica's `lsn`.
  - An engine that acks once its log is written, applies a moment later,
    and reports the applied LSN is correct, but fails here.
  - A separate apply thread or a batched apply loop both behave this
    way.
- **Mock (`r18_mock_apply_lag`):** a per-node sync engine that acks on
  receipt and applies after a set lag. 10 runs per row.

| Apply lag | Lite fails | Full fails |
|---|---|---|
| 0 ms | 0/10 | 0/10 |
| 1 ms | 8/10 | 0/10 |
| 5 ms | 6/10 | 0/10 |
| 20 ms | 10/10 | 0/10 |

- **Why the full scenario passes:** at the same point it uses `wait_lsn`,
  which waits up to 10 s.
- **Wrong message:** it says "promoted replica", but no `PROMOTE` has been
  sent yet.
- **Fix:** use `wait_lsn(&addr(ports, 1), commit_lsn, Instant::now() +
  Duration::from_secs(2), "the replica did not apply the acked COMMIT")`.
  Dropping the check also works:
  - the freeze already proves the ack waited for the replica;
  - `read_acked` after `PROMOTE` proves no acked row was lost.

## N2. LOW (ticket 19): Docker failures inside the scenario count against the model

- **Where:** the pipeline's control closure for ticket 19
  (`pipeline.rs:4087`). A cluster that
  can't start is an infra fault (I4). But the closure turns these
  failures into plain error strings:
  - `docker pause` and `docker unpause`;
  - `docker kill`;
  - the rejoin's `docker run`.
  Each string becomes `late_failure` for stage 19, so the model is graded
  as failed.
- **Why it matters more now:** `.29` adds four Docker calls to each full
  ticket-19 check and two to each lite check. The check reruns in every
  later verification.
- **Fix:**
  - let the closure set a flag (a `Cell<bool>`) when a Docker action
    fails;
  - after the scenario, set `infra_error = true` when the flag is set, as
    for the start.

## N3. LOW (ticket 19): state the freeze length and what moves the LSN, and grade the no-election rule

- **The freeze length:**
  - The full prompt says "a 1 s freeze must not trigger an election". The
    measured freeze is 1.05-1.18 s.
  - A failure detector counts from the last message it received, so it
    sees the freeze plus up to one heartbeat interval.
  - A candidate that picks a 1.5 s timeout with 0.5 s heartbeats can
    still elect during the freeze.
  - The prompt also doesn't say how long `COMMIT` may block.
  - The scenario checks "must not trigger an election" only by
    accident. It reads both replicas' roles after the freeze, then
    ignores them. A replica that elects itself during the freeze is
    caught only if it has the higher port, as a wrong winner. With the
    lower port it can pass, since after the kill it is the only
    primary.
- **The LSN at rest:**
  - The full scenario needs an idle cluster's LSN to stay put. It
    requires equal replica LSNs before the kill, then expects the lower
    port to win.
  - The prompt only says the LSN "only grows". An engine that logs
    heartbeat or no-op entries is correct, but fails with "the replicas
    did not converge" or with an unexpected winner.
- **Suggested wording:**
  - full, item 3: "The freeze lasts up to about 1.5 s; `COMMIT` blocks
    (no `OK` and no `ERR`) until a replica is back, then answers `OK`.
    Use a failure-detection timeout of at least 3 s."
  - full and lite, item 1: "`lsn` advances only when a write commits."
- **Code:** where the scenario reads `l1` and `l2` after the freeze, also
  require both roles to be `replica`. An error there would read "the
  freeze triggered an election".

## N4. LOW (ticket 12, since `.18`): the transfer check races the writers

- **Where:** `concurrency::transfers` needs at least 10 reader snapshots
  (`MIN_SNAPSHOTS`, `concurrency.rs:34` and `:334`). The readers stop as
  soon as the 4 writers finish their 160 transfers. A faster engine
  finishes sooner and leaves the readers fewer snapshots.
- **Seen in the gates:** the other agent's serial run failed
  `the_scenario_repeats_cleanly` with 7 snapshots (TODO_CAMPAIGNS,
  round-17 gates). It was written off as an unrelated timing flake. It is
  this race, in the grading code itself.
- **Measured (`r18_snapshot_counts`):** on the in-process mock, 30 runs
  per mode, while the full suite was running:

| Mock | Min | p10 | Median | Below 10 |
|---|---|---|---|---|
| serializable, readers don't block | 7 | 15 | 23 | 1/30 |
| one global lock (the lite design) | 9 | 17 | 24 | 1/30 |

- **Effect on real engines:**
  - Through Docker, every round trip is slower for writers and readers
    alike. A real candidate usually gets more snapshots than the mock.
  - A fast engine on a busy host can still fail ticket 12.
  - Ticket 12 reruns as a late check. So the failure can also land on any
    later ticket's verification, as a regression of ticket 12.
- **Fix:** each writer sleeps about 1 ms after each transfer.
  - The writers then run for at least 40 ms, so the readers take dozens
    of snapshots while transfers are in flight.
  - That keeps the check's power, and it also cures the unit test.

## N5. Nits

- **Late-check skip message:** when a *late check* failed, rather than a
  differential stage, the later skips read only "not run: late checks
  skipped". `failed_stage` (`pipeline.rs:4385`) is computed before
  `late_failure` is applied.
  Fall back to `late_failure`'s stage.
- **Sentinel variants:** the sentinel block (`pipeline.rs:4437-4441`)
  still builds the lite variants inline. It has the same logic as
  `suite_variants`, so use that.
- **Stale ports in the proposal:** `docs/PROPOSAL-sdb-sql.md:343` still
  says the nodes run on ports 7101-7103. Since `.28` (I4) they use the
  worker's `port+60..62`, which is 5540-5542 on worker 0.
- **Residual, no action:** an engine with semi-sync fallback ("wait for a
  replica, but ack anyway after T") passes when T is longer than the
  freeze. A bounded freeze can't see past T. Note it as a known residual.

## Ops

- The release binary embeds `.29`. The services restarted at 15:03:18 and
  are active.
- No probe containers or networks are left. The reviewer worktrees stay in
  the scratchpad.
- **Keep master's working tree unchanged while a cell runs.**
  - `scripts/sql-cell.sh` builds the release binary once and then runs
    `campaign run` in the foreground.
  - The campaign reads each ticket's `prompt.md` from disk when that
    ticket starts (`campaign.rs`, `prompt_path`).
  - So an edit on master mid-cell would put `.30` prompts in front of a
    `.29` verifier. N3 changes the ticket-19 prompts.
  - The other agent commits on master and restarts the services after
    each fix (12:07 and 15:03), so prepare `.30` in a separate worktree.

## `.30` (round-18 fixes): committed, merged and live

- **Where:** commit **`7bb4955`** "Round-18 review fixes (rules
  `2026-10-19.30`)", 10 files, +369/-63.
  - It was made on branch `round18-fixes`, prepared in a scratch worktree
    while the cell ran, and fast-forwarded into master.
  - With the operator's go-ahead, the release binary was rebuilt (it
    embeds `.30`) and the services were restarted at **17:43:52**.
  - Both are active, the engine is idle, and the GUI answers.
- **Other agent:** don't implement N1-N6 again. This change set covers
  them.
- **The changes:**
  - **N1:** the lite waits for the replica's `lsn` to reach the acked
    commit, up to the election budget. The report takes its LSN from
    `wait_role` after `PROMOTE`.
  - **N2:**
    - `HaAction::node()` and `SandboxManager::ha_node_state()` are new.
    - `repl_control_failure_is_infra` decides: a failed pause, resume or
      kill is an infra fault unless the node's container already exited
      or is dead; a failed rejoin always is.
    - The pipeline then logs `[INFRA] REPL cluster control failed` and
      sets `infra_error`.
  - **N3:**
    - The full scenario fails with "the freeze triggered an election"
      when a replica isn't `replica` after the freeze.
    - Both prompts and the proposal say: the freeze stops the processes
      and connections stay open; it lasts up to about 1.5 s; `COMMIT`
      blocks with neither `OK` nor `ERR`; `lsn` grows only when a write
      commits.
    - The full prompt adds the 3 s failure-detection floor.
  - **N4:** `WRITER_PAUSE` is 1 ms, after each successful transfer.
  - **N5:**
    - the skip message falls back to the failed late check's stage;
    - the sentinel uses `suite_variants`;
    - the proposal's ticket-19 ports are current.
  - **N6** (found in the cell): `classify_failure` treats "not run: ..."
    like "skipped due to failure". It takes the detail and stage from the
    first *real* failure, and rule 5 (own_stage) looks only at real
    failures.
  - **Rules and docs:** rules `2026-10-19.30` with a history entry, and a
    TODO_CAMPAIGNS round-18 entry.
- **New tests:**
  - mock modes `ApplyLag` (must pass, full and lite) and `ElectsOnFreeze`
    (must fail);
  - a lite async-ack test;
  - `repl_control_failures_split_harness_faults_from_crashes`;
  - in the classifier test, the cell's two shapes:
    - own stage plus "not run" late checks must read `own_stage`;
    - a real regression plus "not run" must name only the real one.
  - On `.29` the scenario passes the `ElectsOnFreeze` mock, which is the
    gap N3 closes.
- **Gates (all green):**
  - fmt and clippy are clean.
  - Full suite (16:57-17:32, after the cell):
    - lib **975 passed, 0 failed**, 6 ignored;
    - bins 60; other targets 2.
    - Every live proof passes: 13-18, 19 full and lite, 20 full and lite,
      and both sensitivity end-to-end tests.
  - `the_scenario_repeats_cleanly` passed 20 of 20.
  - Live freeze probe (`r18_live_freeze`) on `.30`:
    - the unchanged reference passes 3 of 3 (full) and 3 of 3 (lite);
    - the asynchronous variant is caught 4 of 4;
    - replicas stay frozen 1.00-1.16 s.
  - With N4, the transfer check's snapshot counts on the same mocks
    (30 runs each) are:

| Mock | Before | With N4 |
|---|---|---|
| serializable | min 7, median 23 | min 121, median 125 |
| one global lock | min 9, median 24 | min 118, median 124 |

## Calibration cell on `.29` (2026-10-20 15:42-16:52)

DeepSeek v4.1 flash, omp/high, $0.75 / 120 min, HEAD `8f4338d`
(`runs/campaign-launch-20261020_154206.log`, rollup
`runs/campaign_sdb-sql_openrouter_deepseek_deepseek-v4.1-flash-1017_20261020_154208/campaign.json`).
It was the first real cell since `.17`.

**Result:**

- reached **ticket 19**, with **34.6 of 76 points** and 13 regression-free
  tickets;
- spent **$0.748**, so the cap stopped it (`cap_reached: true`);
- took **70.8 min** of the 120 min;
- tickets per dollar: 17.4.
- The previous best DeepSeek cell (`.17`, 12-ticket ladder) reached 8
  with 13 of 26 points.

| Ticket | Outcome | Cost | Minutes |
|---|---|---|---|
| 0-7 | pass (full) | $0.251 | 22.3 |
| 8 | fail, full and lite (joins; `ERR syntax`, regressions of 0 and 4) | $0.065 | 6.5 |
| 9 | fail (constraints: a write that should fail was accepted) | $0.053 | 4.6 |
| 10 | pass | $0.054 | 5.2 |
| 11 | fail, full and lite (regressions of 6 and 7) | $0.076 | 6.6 |
| 12 | full failed ($0.085, 9.5 min); **lite passed** (global lock) | $0.094 | 10.6 |
| 13 | pass | $0.041 | 4.9 |
| 14, 15, 16, 18 | skipped: needs 8 or 11 | - | - |
| 17 | pass (disk-full incident) | $0.021 | 1.7 |
| 19 | **pass, full** (3 nodes) | $0.094 | 7.3 |
| 20 | not attempted: cap reached | - | - |

**What it shows about the harness:**

- **No faults.** Every late check ran against a real engine (a Python
  `server.py`) without an infra error:
  - the crash ladder: SIGKILL, torn tail, double restart;
  - transaction isolation;
  - the lite concurrency checks;
  - the disk-full incident;
  - replication;
  - the upgrade sentinels.
- **Ticket 19 under the J1 freeze test:**
  - Round 0 got through the seed, the read-only refusal and the frozen
    `COMMIT`: the engine's commit waited for a replica.
  - It then failed the election, because its own election thread crashed
    (`KeyError`).
  - Round 1 fixed the crash and passed: node 5541, the lower port, was
    elected at LSN 12, the new write and the rejoin converged, and the
    post-failover suite passed.
  - So the freeze test did not false-fail a real synchronous engine.
- **One new reporting bug, N6.** The failure classifier blames the late
  checks the pipeline did not run. The board's cause for tickets 8, 9 and
  11 reads "broke tickets 5,6: not run: late checks skipped after stage N
  failed". Grading is not affected. The fix is in `.30`.
- **A dormant nit, N7.** The old key-value campaign's throughput gate
  (`pipeline.rs` 8b, "acceptance includes stage 12") still runs on
  sdb-sql, whose ticket 12 is concurrency.
  - It logged "throughput gate skipped: no genesis baseline recorded" in
    four verifications.
  - It can't fire today: sql manifests carry no throughput, so
    `baseline.json` is never written.
  - Scope it to the old campaign before anyone adds a sql throughput bench.

**What it shows for M5:**

- **Spend:**
  - Failed attempts took $0.280, 37% of the spend.
  - Spend was 32% of the attempted tickets' budgets ($2.33 of the
    ladder's $3.20). The earlier cells' range was 20-45%.
- **Coverage:** the cap was reached with 49 min of wall time left.
  - The cell got to ticket 19 only because the needs skipped 14, 15, 16
    and 18. A model that passes 8 and 11 attempts those four first.
    Their budgets total $0.67, so at this rate they cost about $0.21, and
    $0.75 runs out before ticket 19.
  - At this model's rate, the five unattempted tickets ($0.87 of budget)
    cost about $0.28 more. So a full ladder costs about $1.03 at this
    rate.
- **The options:**
  - raise the cap to about $1.25-1.50 with a 150-180 min wall, so a
    model of this class can attempt every ticket;
  - keep $0.75, and accept that reaching tickets 14-20 depends on the
    needs as much as on the model.

## Haiku 5.5 cell on `.31` and the cap overshoot (2026-10-20/21)

Haiku 5.5 (`openrouter/anthropic/claude-haiku-5.5`):

- **omp/max (19:56-20:01): a fault.** Turn 2 produced 65,536 output tokens,
  which truncated the write call into invalid JSON. The next request sent
  assistant turns without their thinking blocks; OpenRouter answered "400
  Provider returned error" and omp exited.
- **omp/high (20:03-21:46, 103 min):**
  - result: reached ticket 9, 17.8 of 76 points, 10 regression-free
    tickets;
  - passed 0-4, 5 (lite), 6, 7, 8 and 9; ticket 10 failed;
  - **total $1.0495 against the $0.99 cap.**

Haiku passed joins (8) and constraints (9), which DeepSeek `.29` failed.
It spent about twice as much per ticket, so it ran out of money at
ticket 10. Its largest single call cost $0.033, against $0.0225 for
DeepSeek.

### The overshoot (fixed in `.32`, landed as `8beea56`)

- **Repair rounds got the whole budget again.**
  - Each round's limit was `Some(config.budget_usd)`.
  - Ticket 9: round 0 metered $0.1307 of its $0.12 budget, and the repair
    round another $0.0552.
  - Ticket 10: round 0 metered $0.0364 of $0.02, and the repair another
    $0.0313.
- **The cap was only checked between tickets.**
  - Ticket 10 started on $0.0347 of headroom, above the $0.02 floor.
  - Its compute floor ($0.0264: 542 s plus tokens) was never part of any
    in-session limit.
- **The fix (rules `2026-10-19.32`):**
  - A repair round gets what the earlier rounds metered off the attempt's
    budget, and none starts on less than $0.02.
  - Every round, in OMP and in opencode, stops while the session runs at
    the cell's remaining *effective* headroom: metered API spend plus the
    time and token compute floor.
  - That headroom is net of a reserve: the cell's largest call so far, at
    least $0.02. A ticket or lite attempt starts only with the reserve
    plus $0.02 left.
  - Replayed on the Haiku cell, ticket 10 is not started ($0.0347 <
    $0.0333 + $0.02), and the cell ends at $0.9553.
- **Score impact:** a ticket whose round 0 runs out of budget is no
  longer repaired. Haiku's ticket 9 (3 points) and DeepSeek `.29`'s
  ticket 19 (7 points) passed only that way. Repairs after a turn-cap or
  READY stop are unchanged while budget remains.
- **Low (accounting, not fixed):** the recorded per-round `cost_usd` in
  the rollup is the compute floor only. The API share is in the
  attempt's manifest. The new `[COST]` line prints both.

## Next steps

1. **Done:** `.30` committed (`7bb4955`), merged to master, rebuilt and
   restarted at 17:43:52.
2. **M5 decided (operator, 2026-10-20): the cell cap is $0.99 / 150
   min.**
   - It is the highest budget that keeps every cell under $1, which is
     the README's "Under-$1" claim.
   - Prepared as rules `2026-10-19.31` on branch `m5-cap`:
     `campaign::CELL_BUDGET_USD` / `CELL_WALL_MIN`, used by the console
     launcher, the engine's start default and `scripts/sql-cell.sh`.
   - Committed as `64aaf97`, merged and live since 18:57:25 (rules `.31`).
   - First cell on it: MiniMax M3.1 flash (free), omp/max, 18:57-19:06.
     It failed ticket 0: it used its 28 turns probing the reference and
     never wrote `start.sh`. API spend $0; the cell total was $0.029.
   - Console fix `9f2ff24`, live since 19:11:18:
     - a running cell shows on the Board before its first ticket (the
       list endpoint now sends its stub rollup);
     - Board groups are per rule set, so the 26- and 76-point ladders
       never share a row or a median.
3. **Then normal cells on `.30`.** A second DeepSeek sample would show the
   variance. A stronger model would exercise 14-16, 18 and 20, which no
   real candidate has attempted yet.
4. **Later:**
   - N7: scope the old campaign's throughput gate.
   - This cell's rollup keeps the misattributed causes it recorded. Only
     cells graded on `.30` get N6's fix.

---

# Round 17 (closed)

# Review: round-16 fixes (2026-10-20, round 17)

Reviewer: Claude Code. Scope: `7f78977` "Round-16 review fixes (rules
`2026-10-19.28`)", the only commit after `887b94e`.

The stashes were not touched. Nothing was fixed or committed. All
experiments ran in scratch worktrees at `7f78977`.

**Verdict:**

- **Fixed and correct:** I1, I2, I3, I4 and L-b to L-e. The experiment
  containers and their network are gone.
- **I5 is not fixed (J1, HIGH).** The commit uses the approach that the
  round-16 addendum (T1) warned against.
  - The new sync check can't see asynchronous replication. The reference,
    patched to ack a commit before any replica has it, **passes the 3-node
    scenario 3 of 3 times** live.
  - The new election check fails a correct engine. When the lower-port
    replica is behind at the kill, the harness reads the LSNs after the
    winner has already caught it up. **The check failed 20 of 20 times**
    in a mock.
  - A pause-based test separates the two cases 3 of 3 times each on the
    live reference, with no spurious election. The design is under J1.
- **New MEDIUM (J2):** both ticket-19 scenarios now ask a replica to
  refuse a write to a table that doesn't exist yet. A correct engine that
  looks the table up before it checks its role fails (mock, full and
  lite).
- **New MEDIUM (J3):** ticket 15's new boot probe gives the reverted
  server **200 ms** to answer its first `SELECT 1`, and about 5 s to start
  (the harness allows 25 s everywhere else). When the probe fails, the
  check counts as "inconclusive", and ticket 15 fails.
- Most of the other addendum items weren't done: tests, the probe wait
  and the wording. See the status table.
- **Ops:**
  - The other agent rebuilt and restarted the services on `.28` at
    12:07:02, 8 s before the commit.
  - No cell has run on `.18`-`.28`.
  - **Don't launch a cell until J1 and J2 are fixed.** Ticket 19 needs
    only 6, so any run that passes 0-6 attempts it.

## Test results (HEAD `7f78977`, reviewer worktree, niced)

- `cargo fmt --check`: clean. `cargo clippy --all-targets -D warnings`:
  clean.
- `cargo test` (default parallelism, 12:11-12:45):
  - lib **965 passed, 0 failed**, 6 ignored, 1967 s;
  - bins 60 passed; other targets 2 passed.
  - Every live proof passes: 13-18, 19 full and lite, 20 full and lite.
    So does the ticket-15 sensitivity end-to-end test.
- Probes:
  `/tmp/claude-1000/-home-ubuntu-subdollarbench/b1b03a05-74a3-453d-b2d7-842ae849a2b9/scratchpad/round17-probes.patch`.
  - It adds an `r17_probes` module to `replication.rs` with five
    `#[ignore]` tests: three mocks, and two live Docker probes on the
    worker-41 and worker-42 ports.
  - The J3 check is the probe's own Python, extracted from `pipeline.rs`
    and run against a dummy server.
  - The 88 ms kill time was measured on a throwaway container in the
    round-16 addendum.

## Round-16 status

| Item | Status | Notes |
|---|---|---|
| I1 | **Fixed** | The post-failover suite calls `run_stage_with_variants` with the main suite's variants. |
| I2 | **Fixed** | I re-checked every statement each scenario sends against its `needs` closure: all are covered. |
| I3 | **Fixed** | The 5 s cancel applies only in ticket 18's restart. The reference defaults to no timeout. |
| I4 | **Fixed** | The nodes use `port+60..62`. A failed `docker run` is an infra fault, and readiness is still graded by the scenario. |
| I5 | **Not fixed** | See J1. The commit message, the `.28` history, the proposal and TODO_CAMPAIGNS all describe it as fixed. |
| L-a | Partial | There is a boot probe now, but see J3. The residual (a clean reverse that only removes dead code) stays a known LOW. |
| L-b | **Fixed** | The `28P01` probe is in, and a missing image is a fault. Residuals: K1 and K2. |
| L-c, L-e | **Fixed** | |
| L-d | **Fixed** | The message wording is left (K3). |
| Addendum T1 | Not done | J1 |
| Addendum T2 | Not done | J2. The ordering bug is now in the full scenario too. |
| Addendum T4 | Not done | J3 |
| Addendum T5 | Not done | K2 |
| Addendum T6, T7 | Not done | K3 |
| Addendum T8 | Half done | The containers are removed. `postgres:16-alpine` is still not in the README prerequisites (K3). |

---

## J1. HIGH (ticket 19): the sync-commit check doesn't work, and the new election check fails correct engines

**What the commit does:**

- Full scenario:
  1. It checks that the replicas refuse a write.
  2. It seeds the table, kills the primary as soon as `COMMIT` is acked,
     and polls until exactly one survivor is primary.
  3. It takes the expected winner from the LSNs read in that same poll.
  4. It requires the winner's LSN to be at least the commit's LSN.
- Lite: it kills the primary right after the ack, sends `PROMOTE`, then
  requires the promoted node's LSN to be at least the commit's LSN.

**Problem 1: the kill is far slower than replication.**

- `docker kill -s KILL` takes about 88 ms on this host.
- The primary stays alive through that window, and an asynchronous engine
  replicates inside it. The reference replica pulls every 50 ms; a
  push-based engine ships in under 1 ms.
- Live probe (`r17_live_kill_after_ack`): a real reference cluster in
  Docker, started and killed exactly as the pipeline does, 3 runs per
  cell.

| Reference variant | 3 nodes | 2 nodes (lite) |
|---|---|---|
| unchanged: `COMMIT` waits for a replica's ack | 3/3 pass | 3/3 pass |
| `COMMIT` acks before any replica has the commit; replicas pull every 50 ms | **3/3 pass** | 1/3 pass |
| same, but replicas pull every 300 ms | 1/3 pass | 0/3 pass |

- The asynchronous variants were caught only when the promoted node was at
  **LSN 0**. That node had not received even the `CREATE TABLE`, because
  nothing in either scenario waits for a replica to reach any LSN above
  the initial 0 before the kill.
- Once the J2 fix makes the scenario wait until the replica has the table,
  the lite will catch asynchronous replication as rarely as the full
  scenario does.

**Problem 2: the LSNs are read after the election.**

- Setup (`r17_mock_post_election_lsn_race`): a mock cluster with
  per-node LSNs, modelling a correct engine:
  - `COMMIT` is acked once the faster replica has it;
  - at the kill, the lower-port replica is behind by the last
    transaction;
  - the higher-LSN replica wins, which is correct, and ships its tail to
    the other survivor at once.
- The harness reads both LSNs in the poll that first sees one primary.
  They are already equal, so it expects the lower port.
- Result: **20 of 20 failed** with "elected node … (index 2), expected the
  highest-LSN/lowest-port survivor index 1".
- With no lag, the same mock passes both scenarios (`r17_mock_controls`).
- This was my round-16 suggestion ("take the winner from the LSNs read
  after the kill"), and the addendum's T1 corrected it. The LSNs can still
  move after the primary is dead, because the new primary ships its tail
  to the other survivor.

**Fix:** test sync commit by freezing the replicas, and keep the election
check on equal LSNs. This is validated live (`r17_live_pause_commit`).

1. Create the table and the 10 seed rows. Wait until every replica has
   reached that LSN. Then check that each replica refuses a write (J2).
2. `docker pause` every replica.
3. Send `BEGIN` and the 5 inserts, then `COMMIT` with a 1 s read deadline.
   An answer inside that second fails with "COMMIT acknowledged while every
   replica was frozen".
4. `docker unpause` one replica. The pending `COMMIT` must answer `OK`
   within 5 s. Then unpause the others.
5. Full scenario: wait until both replicas reach the commit's LSN. Their
   LSNs are then equal, so the lower port must win, which is deterministic.
   Then do the kill, the acked rows, the new write and the rejoin, as
   before.
6. Lite: kill the primary, send `PROMOTE` and check the 15 rows.

- **Live result on the reference:**
  - unchanged reference: no `COMMIT` answer within 1.01-1.04 s with both
    replicas frozen; `OK` arrives as soon as one replica is unpaused. 3 of
    3 runs.
  - the variant that acks first answers `COMMIT` in 0 ms with both
    replicas frozen. Caught 3 of 3 times.
  - In every run the roles were still `primary`/`replica`/`replica`, all
    at the same LSN, 3 s after the unpause. So a 1 s freeze causes no
    spurious election in the reference.
- **Plumbing:**
  - add `HaAction::Pause(i)` and `HaAction::Resume(i)`;
  - the pipeline maps them to `docker pause`/`unpause` of
    `subdollar-ha{i}-…`;
  - the mock gets a pause flag per node and an asynchronous mode, which
    must fail.
- **Prompt (full and lite):** add one sentence: "the harness freezes the
  replicas for about 1 s; `COMMIT` must not be acknowledged until one of
  them has it, and a 1 s freeze must not trigger an election".
- **Docs:** correct the `.28` history line, the TODO_CAMPAIGNS round-16
  entry, the proposal ("highest `lsn` read after the kill") and the module
  doc. All of them currently describe the immediate-kill check as proof of
  sync commit.

## J2. MEDIUM (ticket 19, regression): replicas must refuse a write to a table that doesn't exist yet

- **Where:** both scenarios now call `replica_refuses` before `seed_rows`
  creates the table. The full scenario does this for both replicas; the
  lite was already like this in the round-16 addendum (T2).
- **Effect:**
  - The check sends `INSERT INTO ha_<seed> VALUES (999, 'forbidden')` to
    a replica that has no such table, and requires `ERR readonly`.
  - An engine that looks the table up before it checks the role answers
    `ERR no_table` and fails ticket 19. An example is a write guard in the
    storage layer, below the executor.
  - The contract's "replicas refuse writes" doesn't say which error comes
    first.
  - The reference passes only because it refuses by keyword before parsing
    (`ref_server.rs:1031`).
- **Mock:** `r17_mock_replica_checks_table_first` fails both the full and
  the lite scenario with "a write on a replica answered Err { class:
  \"no_table\" … }, expected ERR readonly".
- **Fix:** step 1 of the J1 fix. Create the table, wait for the replicas,
  then check the refusal.

## J3. MEDIUM (ticket 15): the boot probe gives the reverted server 200 ms to answer

- **Where:** the L-a boot probe in `run_tests_gate_blocking`. It connects
  with `socket.create_connection((…, 7123), 0.2)`, and Python keeps that
  0.2 s as the socket's timeout. The following `readline()` therefore
  waits at most 200 ms for the answer to `SELECT 1`.
- **Check:** I extracted the probe's Python from `pipeline.rs` and ran it
  against a dummy server.
  - First answer after 50 ms: exit 0.
  - First answer after 250 ms: exit 1, `TimeoutError: timed out`.
- **Second limit:** the connect loop gives the server about 5 s to listen
  (100 × 50 ms), while the harness waits 25 s for every other start
  (`wait_for_port(port, 25)`).
- **Effect:**
  - A failed probe is graded "sensitivity is inconclusive", which **fails
    ticket 15**.
  - So a reverted server that is slow to start, or slow to answer its
    first query, fails ticket 15 although its suite is sensitive.
  - Typical causes are opening its store on first use, imports, or CPU
    contention from parallel cells.
- **Fix:**
  - call `s.settimeout(10)` after connecting;
  - poll for 25 s, matching the rest of the harness;
  - raise the `run_workspace_command` cap from 30 s to about 45 s.

## Low

- **K1. Ticket 20's wrong-password probe.**
  - It allocates whatever message length the server sends:
    `vec![0u8; (n - 4).max(0)]`, so up to 2 GiB.
  - `PgConn::read_msg` rejects anything outside 4..=64 MiB. Use the same
    check.
  - An `AuthenticationOk` that arrives after the wrong password is
    reported as "accepted the connection before a password". It should
    read "accepted the wrong password".
- **K2. Negative tests are still missing (addendum T5):**
  - a PG mock that accepts any password;
  - the `unique_keys = false` path;
  - an asynchronous replication mock;
  - a replica that checks the table first.
  - J1 and J2 come with the last two.
- **K3. Leftovers:**
  - The I1 driver test still sits between
    `persistence_and_sentinel_profiles_follow_the_passed_tickets` and its
    doc comment (T6).
  - "not run: an earlier stage failed" also appears when a *later* stage
    failed (T7).
  - `postgres:16-alpine` is still missing from the README prerequisites,
    next to the `subdollar-sandbox` build (T8). Without it, every ticket-20
    attempt faults.

## Ops

- The release binary embeds rules `.28`. The services restarted at
  12:07:02 and are active.
- The experiment containers (`rha1`-`rha3`, `refpg`, `refsec-bench`) and
  their network are gone.
- No cell has run since `.17`, and M5 is still open.
- **Hold cells until `.29`:**
  - under `.28`, an asynchronous engine can pass ticket 19's sync-commit
    item;
  - a correct engine can fail ticket 19 on J1's election check or on
    J2's table-first ordering.

## Suggested order

1. J1 and J2 together in `replication.rs`:
   - the pause-based test and the reordered refusal check;
   - `HaAction::Pause` and `HaAction::Resume` in the pipeline and the
     mock;
   - the asynchronous and table-first mock modes;
   - the prompt sentence and the docs;
   - rules `.29`.
2. J3: the boot probe timeouts.
3. K1-K3.
4. Gates: the full suite, plus the 19 full and lite live proofs on the
   pause test.
5. My review of `.29`. Then the capped cell and M5, as planned in the
   round-16 addendum.

---

# Round 16 (closed)

# Round 16 addendum: check of the fix in progress (2026-10-20)

Reviewer: Claude Code. I read the uncommitted diff on top of `887b94e`:
15 files, +411/-125, rules `.28` in the history. I did not build or test
it, because the other agent's clippy run and the ticket-19 live proof were
running. The round-16 findings are below this addendum.

## Status per finding

| Item | Status | Notes |
|---|---|---|
| I1 | **Done** | The post-failover suite calls `run_stage_with_variants` with the same `variants` as the main suite. The new test checks the profile, not the call site (T6). |
| I2 | **Done** | `needs` widened for 12, 14, 16, 18 and 20. 16, 17, 19 and 20 use plain DDL and full-table reads. `23505` is checked only when ticket 9 is in `wanted`. The proposal lists each scenario's statements. I re-checked every statement each scenario sends against its needs closure, and all are covered. Ticket 18's `ORDER BY` base is only mutated, and any answer is accepted. |
| I3 | **Done** | `SDB_SQL_STATEMENT_TIMEOUT_MS=5000` is set only in the ticket-18 restart. The prompt and proposal say so. The reference defaults to no timeout. |
| I4 | **Done** | The nodes use `port+60..62`. A start failure is an infra fault, which is correct: `start_sql_ha_nodes` fails only on `docker run`. Readiness is checked by the scenario, so a server that can't run as a cluster still fails as a model failure. |
| I5 | **Not done, and the lite change doesn't work** | The 3-node `scenario` is unchanged: it still waits for both replicas before the kill. The rules history already claims the fix. See T1 and T2. |
| L-a | Partial | There is now a boot probe on the reverted copy. Its wait is too short (T4). |
| L-b | **Done** | The `28P01` probe is in, and a missing `postgres:16-alpine` is now a fault. There is no negative test yet (T5). |
| L-c | **Done** | The AUTH scope is in the prompt. |
| L-d | **Done** | Skipped differential stages no longer count as regressions. Stages 5, 6 and 11 flip to `not run` when their late check didn't run. The message is wrong in one case (T7). |
| L-e | **Done** | 25 ms pause between the settle reads. |

## TODO before committing `.28`

**T1. I5, 3 nodes: my suggested fix doesn't work. Use a freeze instead.**

- The kill is too slow. `docker kill -s KILL` takes **88 ms** on this host
  (measured on a throwaway container). Asynchronous replication on
  localhost takes well under 1 ms. So "kill right after the ack" can never
  catch an asynchronous engine.
- "Expected winner from post-kill LSNs" races with the election.
  - An engine that notices the dead connection elects within milliseconds,
    then ships its tail to the other survivor. Both LSNs become equal, the
    harness sees a tie and expects the lower port.
  - So the harness rejects a correct higher-port winner.
- **Test it deterministically instead:**
  1. Seed the rows and wait until both replicas reach the LSN, as now.
  2. `docker pause` both replicas (about 45 ms each here).
  3. Send `BEGIN` and the inserts, then `COMMIT` with a 1 s read deadline.
     If an answer arrives within that second, fail: "COMMIT was
     acknowledged while no replica could receive it".
  4. `docker unpause` the lower-port replica. The pending `COMMIT` must
     answer `OK` within 5 s. Then unpause the other replica.
  5. Continue as now. Wait until both replicas reach `commit_lsn`. Their
     LSNs are then equal, so the lower port must win. Then kill, check the
     acked rows, new writes and the rejoin.
- **Mock:** add `HaAction::Pause(i)` and `HaAction::Resume(i)`. Add an
  asynchronous mode that acks while every replica is paused; it must fail.
- **Prompt:** say that the harness freezes the replicas for about 1 s and
  expects `COMMIT` to wait. Ask for a failure-detection timeout of at least
  3 s.
  - The reference replica elects after 1.5 s of silence (`last_ok > 1.5`).
    After a 1 s freeze that is too close, so raise it to 3 s. That still
    fits the 10 s failover budget.
- Correct the `.28` history line for I5 to match what ships.

**T2. I5, lite (2 nodes): fix two problems.**

- **New bug:** `replica_refuses` now runs before `seed_rows` creates the
  table.
  - The INSERT on the replica targets a table that doesn't exist yet.
  - An engine that checks the table before the role answers with an error
    other than `readonly`, and fails although it is correct.
  - The reference checks the role first, so the live proof doesn't show
    this.
  - Fix: create the table, wait until the replica reaches that LSN, check
    the refusal, then run the transaction.
- The new check ("kill right after the ack, then `lsn >= commit_lsn`")
  can't catch asynchronous replication, for the same 88 ms reason.
  - Use the freeze: pause the replica, and `COMMIT` must not answer within
    1 s.
  - Unpause the replica: `COMMIT` must answer `OK`.
  - Then kill, `PROMOTE` and check the 15 rows.

**T3. Order matters: fix T1 and T2 before any cell runs on `.28`.**

- With the new `needs`, ticket 19 needs only 6, and ticket 20 needs 1, 6
  and 7. So mid-ladder cells attempt them.
- Example: the best DeepSeek cell (`20261018_142825`) passed 0-6 and 8.
  On this ladder it would attempt 12, 13, 17 and 19.
- Another (`20261018_075158`) would attempt 13, 17, 19 and 20.

**T4. L-a: the boot probe waits about 5 s (100 × 50 ms).**

- The harness's normal start wait is 25 s (`wait_for_port(port, 25)`).
- So a server that starts in 5-25 s passes every ticket, but makes the
  ticket-15 sensitivity check "inconclusive", which fails ticket 15.
- Fix: poll for 25 s, and raise the `run_workspace_command` cap from 30 s
  to about 45 s.
- `--network none` makes port 7123 container-local, so there is no
  collision between workers.
- The residual stays a known LOW: a clean reverse that only removes dead
  code.

**T5. Add the missing negative tests.**

- A pgwire mock mode that accepts any password must fail with "accepted the
  wrong password".
- With `unique_keys = false`, the scenario must not send the duplicate
  insert.
- Lite 19: a mock replica that answers a missing table before checking
  `readonly` must still pass, after T2.
- The asynchronous mock from T1.

**T6. Optional: make I1 regression-proof.**

- Extract `suite_variants(passed_lite, lite_profile, wanted)` and use it
  for both the main suite and the post-failover suite, then test it once.
  The current test would not notice the call site going back to
  `run_stage`.
- The new driver test was inserted between
  `persistence_and_sentinel_profiles_follow_the_passed_tickets` and its
  doc comment. So the old test lost its comment, and the new test carries
  both. Move it above the old comment.

**T7. L-d wording.**

- The late checks run only after the whole differential suite passes. So
  stages 5, 6 and 11 also flip when a *later* stage failed, and the
  message "an earlier stage failed" is then wrong.
- Suggested text: `not run: late checks skipped after stage N failed`.

**T8. Ops.**

- Stop `rha1`-`rha3`, `refpg` and `refsec-bench` when the experiments are
  done. Four of them publish on `0.0.0.0`.
- Make `postgres:16-alpine` part of the host setup, by pulling it in the
  deploy script. Its absence now faults every ticket-20 attempt.

## After the commit

1. **Gates:** fmt, clippy and the full test suite, including the live
   proofs 12-20, with 19 full and lite on the freeze test.
2. **Review:** I review the fix commit, focused on the post-failover call
   site, the asynchronous mock and the lite ordering.
3. **Services:** rebuild and restart on `.28`, with the operator's
   go-ahead.
4. **One capped cell:** DeepSeek v4.1 flash, the best so far (reached 8,
   13/26 points), at $0.75 / 120 min.
   - It is the first real cell since `.17`. Eleven rules versions and nine
     new tickets have not met a real candidate.
   - Check that tickets 0-11 show no new failure kinds and no faults.
   - Check which of 12-20 it attempts, and that their late checks don't
     fault.
   - Measure the wall time per verification. Ticket 16's 400 MiB scenario
     reruns in every later verification.
5. **Then M5, with real numbers.**
   - So far DeepSeek cells spent **$0.31-0.77 and 22-57 min** for tickets
     0-11. That is about 20-45% of the budgets of the tickets they attempted.
   - At that rate, the 21-ticket ladder costs about **$0.65-1.45**, plus
     the heavier late checks. So the $0.75 / 120 min cap will probably
     stop cells somewhere in tickets 12-18.
   - The options are:
     - raise the cap, for example to $1.50 / 180 min;
     - keep the cap and treat reaching the top as part of the score;
     - cut the per-ticket budgets.

---

# Review: round-15 fixes and tickets 18-20 (2026-10-20, round 16)

Reviewer: Claude Code. Scope: the three commits on master after `8da3778`.

| Commit | Content | Rules |
|---|---|---|
| `c4ddcdc` | round-15 fixes (H1, H2, M1-M4, L1-L5) | `.24` |
| `a7e01f8` | ticket 18, hostile input and limits | `.25` |
| `887b94e` | tickets 19 (replication and failover) and 20 (PostgreSQL wire) | `.26`, `.27` |

The stashes were not touched. Nothing was fixed or committed. All
experiments ran in a scratch worktree at `887b94e`.

**Verdict:**

- **The round-15 fixes are correct.** I re-ran the round-15 probes against
  the new code. Each fix is checked in the table below.
- Tickets 18-20 follow the same pattern as 12-17: a scenario module, mock
  tests in both directions, a reference implementation and a live proof.
  All of them pass here.
- **One new HIGH (I1):** after failover, ticket 19 runs the cumulative
  suite at the *full* variant of every ticket. A cell that passed ticket 8
  on its lite variant is graded on LEFT JOIN. This is the same class of bug
  as G1. Fix it before any cell can reach ticket 19.
- **Four new MEDIUMs:**
  - I2: seven scenario tickets use grammar from tickets outside their
    `needs`. This was already true for 12-17, and I missed it in round 15.
  - I3: ticket 18's 5 s statement cancel conflicts with ticket 16's heavy
    queries.
  - I4: the HA node ports collide with another worker's candidate port.
  - I5: "sync commit" isn't actually tested.
- Ops:
  - The services already run `887b94e` (rules `.27`, 21 tickets, max 76
    points).
  - No cell has run on `.18`-`.27`.
  - M5 is still open: the per-ticket budgets add up to $3.20 against the
    $0.75 cap.

## Test results (HEAD `887b94e`, reviewer worktree, niced)

- `cargo fmt --check`: clean. `cargo clippy --all-targets -D warnings`:
  clean.
- `cargo test` (default parallelism):
  - lib **964 passed, 0 failed**, 6 ignored, 1986 s;
  - bins 60 passed; other targets 2 passed.
  - Every live proof passes: 12-17, 18, 19 full and lite, 20 full and
    lite.
- Probes:
  `/tmp/claude-1000/-home-ubuntu-subdollarbench/b1b03a05-74a3-453d-b2d7-842ae849a2b9/scratchpad/round16-probes.patch`.
  It contains three temporary tests: `g13_unknown_rate`,
  `g13_probe_stage13_on_one_store` and
  `r16_post_failover_profile_ignores_lite`.

## The round-15 fixes

| Finding | Status | How I checked |
|---|---|---|
| H1: PK/UNIQUE drops unmapped | **Fixed** | Unmapped messages in 0 of 2000 stage-13 scripts (was 169) and 0 of 2000 sentinel scripts (was 44). The soak now asserts `unknown` is empty. |
| H2: rename targets not namespaced | **Fixed** | One reference store, 40 stage-13 seeds: 0 failures, 0 faults (was 3 failures and 4 faults). The 20× reference test now covers 0..=13. |
| M1: 64 MiB cap plus 64 MiB swap | **Fixed** | `--memory-swap=64m` gives `memory.swap.max` = 0. Touching 100 MiB is now OOM-killed (exit 137). |
| M2: self-reported coverage | Disclosed | The prompt and the log now say "self-report, not verified". Acceptable as a stated rule. |
| M3: broad sensitivity revert | **Fixed**, residual LOW | `git apply -R --3way` exits 1 on a conflicting reverse, which triggers the logged whole-file fallback, and 0 on a clean one. I confirmed both in a scratch repo. Residual: L-a. |
| M4: 2 MiB writes in ticket 17 | **Fixed** | Both writes are 64 KiB. The second write while full is no longer required, and the volume is filled to the byte. |
| L1: placeholder PASS | **Fixed** | Stages that never ran say `not run: an earlier stage failed`, and `detect_regressions` skips them. Residual: L-d. |
| L2, L3, L4, L5 | **Fixed** | Gauge settle loop, `INDEX SCAN` accepted (mock test), prompt wording, 300-char cap on the error text (test). |

---

## I1. HIGH (ticket 19): the post-failover suite ignores lite variants

- **Where:** the ticket-19 block in `pipeline.rs` calls
  `driver::run_stage(addr, …, &passed, DEFAULT_STEPS, None, None)` with
  `passed = wanted ∩ 0..=13`. `run_stage` (`driver.rs:181`) builds
  `Profile::for_passed(passed)`, which applies no lite narrowing. The main
  suite uses `effective_profile(passed, variants)` instead.
- **Effect:** a cell that passed ticket 8 on `join-inner-only` gets LEFT
  JOIN statements from the HA winner.
  - Measured over 200 seeds at `DEFAULT_STEPS`: **175 of 200** post-failover
    scripts contain a LEFT JOIN. With the effective 8-lite profile the
    count is **0 of 200**.
  - A server built to the lite contract answers those with an error, and
    ticket 19 fails on a feature the ladder never required of this cell.
- **Why the gates missed it:** the 19 live proofs pass no lite variants.
- **Fix:**
  - call `run_stage_profiled` with `effective_profile(&passed, &variants)`,
    where `variants` is `passed_lite` plus the current lite profile, as in
    the main suite;
  - add a test with `(8, ["join-inner-only"])`.

## I2. MEDIUM (tickets 12, 14, 16-20): scenarios use grammar from tickets outside their `needs`

`needs` decides what a cell attempts (otherwise the ticket is
`skipped: needs N`). But the scripted scenarios send statements from
tickets that aren't in the needs chain:

| Ticket | `needs`, and its full chain | Scenario requires | Not in the chain |
|---|---|---|---|
| 12 | 6 → {0, 4, 5, 6} | `WHERE` (1), `ORDER BY` (2), `UPDATE` (3) | 1, 2, 3 |
| 14 | 11 → {0, 1, 3, 4, 11} | `BEGIN`/`COMMIT`/`ROLLBACK` must succeed (6) | 6 |
| 16 | 2, 4 → {0, 1, 2, 4} | `PRIMARY KEY`/`NOT NULL` DDL (9), `BEGIN`/`COMMIT` (6), `COUNT`/`SUM`/`MIN`/`MAX` (7), `JOIN` (8), `CREATE INDEX` (11) | 6, 7, 8, 9, 11 |
| 17 | 5 → {0, 4, 5} | `PRIMARY KEY`/`NOT NULL` (9), `COUNT` (7), `ORDER BY` (2), `WHERE` (1) | 1, 2, 7, 9 |
| 18 | 0 → {0} | `SELECT 1` and parentheses (1), `BEGIN`/`COMMIT` (6), `COUNT` (7), a four-way `JOIN` (8) for the timeout workload | 1, 6, 7, 8 |
| 19 | 6 → {0, 4, 5, 6} | `PRIMARY KEY`/`NOT NULL` (9), `COUNT` (7), `WHERE` (1) | 1, 7, 9 |
| 20 | 1 → {0, 1} | `PRIMARY KEY`/`NOT NULL`, plus duplicate-key enforcement for `23505` (9), `BEGIN`/`COMMIT` (6), `COUNT` (7) | 6, 7, 9 |

- **Effect:** a cell that didn't pass ticket 9 still attempts 16, 17, 19
  and 20. No DeepSeek cell has passed ticket 9 so far.
  - Their first `CREATE TABLE` uses `PRIMARY KEY`/`NOT NULL`, and the
    prompts for 16, 17 and 19 don't show that DDL.
  - Ticket 20 also needs primary-key uniqueness to return `23505`.
  - Ticket 18 can't run its 5 s workload without joins and aggregates.
- This is the G1 problem again: grading work that the ladder told the
  model wasn't required.
- **Fix, per ticket:**
  - remove grammar the scenario doesn't need. For example, use plain
    `id INTEGER, v TEXT` DDL in 16, 17, 19 and 20, and check `23505` only
    when ticket 9 passed;
  - add the tickets a scenario really needs to `needs`. For example: 16
    gets 7, 8 and 11; 18 gets 1, 6, 7 and 8 (or a different slow workload);
    12 gets 1, 2 and 3; 14 gets 6;
  - list each scenario's statement kinds in the proposal, so the next
    ticket is checked against it.

## I3. MEDIUM (tickets 16 and 18): the 5 s statement cancel is unconditional, but ticket 16 runs long statements

- **The conflict:**
  - Ticket 18's contract says "a statement that runs for more than 5 s is
    cancelled and answers `ERR limit`", with no scope.
  - Ticket 16 runs `CREATE INDEX` over 400k rows, a full-table
    `ORDER BY … LIMIT 10` over about 380 MiB, and a 400k-row join, all
    under a 64 MiB cap. Its harness read timeout is 300 s.
- **Effect:**
  - On a slow engine (for example Python reading 380 MiB from disk),
    several of these statements take more than 5 s.
  - Ticket 16's scenario reruns in every later verification, including
    ticket 18's own. Once ticket 18 adds the cancel, ticket 16 answers
    `ERR limit` and regresses inside ticket 18, 19 and 20.
  - Ticket 11's perf gate (200k-row copy and index build) can hit the same
    limit.
  - The reference is SQLite, so the live proofs can't show this.
- **Fix:** scope the budget. For example, the harness sets
  `SDB_SQL_STATEMENT_TIMEOUT_MS=5000` in the ticket-18 restart, and the
  contract says the cancel applies only when that variable is set. The
  alternative is to state in ticket 16 that each of its statements must
  finish within 5 s, which is a much harder ticket.

## I4. MEDIUM (ticket 19, ops): the HA node ports collide with another worker's candidate port

- **Root cause:**
  - The HA nodes use `base = port + 200`, where
    `port = 5480 + 100 × worker`.
  - Worker 0's nodes are therefore on 5680-5682, which includes worker 2's
    candidate port (5680, TCP and UDP). Worker 1's nodes are on 5780-5782,
    which includes worker 3's port.
  - This host runs `SDB_WORKERS=4`.
- **Effect:** with two cells running at once, one of two things happens:
  - `docker run -p 127.0.0.1:5680:5680` fails, and the failure is recorded
    as the *model's* late failure (`[repl] start: …`), not as an infra
    fault;
  - or the other cell's candidate can't bind its port.
- The tests serialize every live proof (`SQL_PERF_LIVE`), so they can't
  show this.
- **Fix:**
  - use a range that no worker's candidate uses, for example
    `worker_port(7101, worker)` as the proposal had, or `port + 60..62`
    inside the worker's own block of 100;
  - treat a cluster that can't start as an infra fault.

## I5. MEDIUM (ticket 19): "sync commit" is never actually tested

- **What the scenario does:** `seed_rows` commits. The scenario then waits
  up to 10 s for **both** replicas to reach the commit's LSN, and only
  after that kills the primary.
- **Effect:** an engine that acks before any replica has the commit
  (asynchronous replication) passes, as long as it replicates within
  10 s. Contract item 3 is exactly the property that separates the two.
- **Fix:** kill the primary right after the `COMMIT` ack, without waiting,
  and require the elected survivor to hold that row. Take the expected
  winner from the replicas' LSNs read after the kill, since they can't
  change once the primary is dead.
- **Related, LOW:** the rejoin starts node 1 on a fresh tmpfs. So a rejoin
  with a divergent log tail (the hard part of a rejoin) is never tested,
  although the prompt reads as if the old primary restarts with its data.

## Low

- **L-a. Ticket 15, residual from M3.**
  - A clean reverse doesn't prove the feature is gone: if a later ticket
    moved the live code, the reverse only removes dead code, and a good
    suite fails "stayed green".
  - The whole-file fallback can leave a server that doesn't boot, which
    passes trivially.
  - Both directions are fixed by checking the reverted copy with the
    harness's own suite (stage 0 passes, stage 7 or 8 fails) before
    grading `./test.sh`. Otherwise call the result inconclusive.
- **L-b. Ticket 20.**
  - The prompt requires rejecting a wrong password with `28P01`, but the
    scenario never sends one.
  - The psql cross-check is skipped when `postgres:16-alpine` isn't local.
    It is present here, but on another host the grade changes. Fail with
    an infra fault instead of skipping.
- **L-c. Ticket 18 prompt and checks.**
  - The prompt doesn't say that AUTH applies only when `SDB_SQL_PASSWORD`
    is set, and the cumulative suites never send AUTH. Say it.
  - The timeout check requires `ERR limit`, so an engine that answers the
    10⁹-row count quickly and correctly (factorized counting) fails.
    That's unlikely with these candidates; just be aware of it.
- **L-d. Regression accounting is inconsistent.**
  - `detect_regressions` now skips `not run:`, but it still counts
    `skipped due to failure in earlier stages`. So one early failure lists
    every later differential stage that passed before as a regression,
    while untested scenario stages are left out. Treat both kinds the same.
  - Stages 5, 6 and 11 keep `PASS` when their late check didn't run.
- **L-e. Ticket 14's settle loop.** It accepts two back-to-back equal
  reads, with no pause between them, so it's weaker than it looks. Fine
  for now.

## Ops

- The release binary was rebuilt and the services restarted on `887b94e`
  at 07:01:14, the same second as the commit. The engine reports rules
  `.27`. Every existing run now sits under "older rules".
- **M5 (operator decision) is still open:**
  - 21 tickets, max 76 points, per-ticket budgets totalling **$3.20**,
    against the $0.75 / 120 min cell;
  - from ticket 18 on, each verification also runs the hostile-input
    scenario, the cluster and the PG restart on top of the tickets 11-17
    checks.
- The other agent's manual experiment containers are still up: `rha1`,
  `rha2`, `rha3`, `refpg` and `refsec-bench`, all mounting `/tmp/refsec`.
  Four of them publish ports on `0.0.0.0` (7391-7393 and 7491-7492). Stop
  them when the experiments are done. I didn't touch them.
- My obsolete `rules-18` branch and worktree are gone (the other agent
  removed them, as recommended).

## Suggested order

1. I1: one call change plus a test. Do it before any cell can reach
   ticket 19.
2. I2 and I3: the needs/DDL table and the scoped 5 s budget. Do them
   before any cell can reach ticket 12 or 16.
3. I4 (port range) and I5 (kill right after the ack).
4. One capped `.27` cell to confirm tickets 0-11 still grade as on `.17`.
   Then the M5 decision.
5. L-a to L-e when convenient.

---

# Round 15 (closed)

# Review: tickets 12-17 (2026-10-19, round 15)

Reviewer: Claude Code. Scope: the six commits on master after `525c159`.

| Commit | Ticket | Rules |
|---|---|---|
| `48d92a6` | 12 concurrency and isolation (+ N5, P1) | `.18` |
| `ff763d8` | 13 schema change (ALTER, differential) | `.19` |
| `2506378` | 14 observability (EXPLAIN, SHOW STATS) | `.20` |
| `abb5d3d` | 15 tests and CI gate | `.21` |
| `335f1b6` | 16 larger than memory | `.22` |
| `8da3778` | 17 incident: disk full | `.23` |

The stashes were not touched. Nothing was fixed or committed. All
experiments ran in a scratch worktree at `8da3778`.

**Verdict:**

- Tickets 12, 14, 16 and 17 are sound scenario stages. Each has mock tests in
  both directions and a live proof. The pipeline wiring (late checks, lite
  variants from `passed_lite`, scratch stores restored afterwards) is
  consistent.
- **Ticket 13 has two HIGH bugs** (H1, H2). Both reproduce against the SQLite
  reference server itself. One faults and poisons the cell about 1 time in
  12. The other fails a correct server once the store has history. **Fix
  both before any cell can reach ticket 13.** Every later ticket reruns
  stage 13, so the risk adds up from ticket 13 to 17.
- Ticket 15's grade proves less than its prompt claims (M2, M3).
  - Coverage is self-reported.
  - The sensitivity revert removes much more than the named ticket.
- The 64 MiB cap is really 128 MiB (M1). Ticket 17 needs statements of 2 MiB
  or more, which no ticket's contract states (M4).
- N5 and P1 from round 14 are done (in `.18`). This makes my `rules-18`
  branch (`da0f4d6`) obsolete. Delete it; don't merge it.
- Ops: no live cell has run on `.18`-`.23`. The ladder's per-ticket budgets
  add up to $2.65 against the $0.75 cell cap (M5).

## Test results (HEAD `8da3778`, reviewer worktree, niced)

- `cargo fmt --check`: clean. `cargo clippy --all-targets -D warnings`:
  clean.
- `cargo test` (default parallelism): lib **937 passed, 1 failed**, 6
  ignored, 808 s.
  - The failure is `test_pipeline_eval_only_sql_stage16_memory_live`:
    `missing "[UPGRADE] sentinel for stage 16 written"`.
  - The log line before it: `[WARN] sentinel for stage 16 not written:
    sentinel script: unmapped oracle messages ["ALTER TABLE …_name drop
    CoLUMN …_txt: cannot drop PRIMARY KEY column: …"]`.
  - This is H1, not a flake of the Docker proofs. The test's seed comes
    from the process id, so it fails on some runs and passes on others.
- The probes below are in
  `/tmp/claude-1000/-home-ubuntu-subdollarbench/b1b03a05-74a3-453d-b2d7-842ae849a2b9/scratchpad/round15-probes.patch`
  (two temporary tests, `g13_unknown_rate` and
  `g13_probe_stage13_on_one_store`). Apply it to reproduce.

---

## H1. HIGH (ticket 13): dropping an indexed PK/UNIQUE column is an unmapped oracle message, which is a verifier fault

- **Root cause, generator:** `alter_probe` arm 2 (`generator.rs`, "drop an
  indexed column") picks any non-unique index. The index can sit on the
  table's PRIMARY KEY column or on a UNIQUE column.
- **Root cause, oracle:** for those columns SQLite answers `cannot drop
  PRIMARY KEY column: "…"` or `cannot drop UNIQUE column: "…"`.
  `oracle.rs:135` maps only `cannot drop column…` and `…after drop column…`,
  so these messages are `Unknown`.
- **Measured** with `build_script`, 2000 seeds each:
  - `Profile::through(13)`, 120 steps (the stage-13 suite): **169 of 2000
    (8.5 %)** scripts carry an unmapped message.
  - 40 steps (the sentinel's size): **44 of 2000 (2.2 %)**.
  - On one SQLite reference store with 40 fresh stage-13 seeds: **4 of 40**
    runs are verifier faults. Two were PK drops and two were UNIQUE drops.
- **Effects:**
  1. Differential suite: `run_stage_profiled` returns
     `Err("unmapped oracle messages…")`. The pipeline records a verifier
     fault, and since round 14 a harness fault stops and poisons the cell.
     Every verification that includes stage 13 draws a new seed, including
     repair rounds and tickets 14-17. A cell that reaches ticket 17 draws
     about 5-10 of them, so it is more likely than not to be poisoned by
     this alone.
  2. Sentinel: the sentinel is silently not written (WARN only), so that
     ticket's upgrade check is lost. This is the failing live test above.
- **Why the gates missed it:** `clean_toy_soak` (toy.rs) runs `run_script`
  on the toy and never looks at `script.unknown`. The "0/3000 soak through
  stage 13" therefore says nothing about unmapped messages.
- **Fix:**
  - Map both messages to `constraint`, which is what the prompt promises
    for PK/UNIQUE drops.
  - Add `assert!(script.unknown.is_empty())` to the soak, and run the soak
    again.

## H2. HIGH (ticket 13): rename targets and the missing-table probe are not namespaced, so a correct server fails on its persistent store

- **Root cause:**
  - `schema::generate` namespaces every identifier with the seed (`s{seed:x}_…`),
    because "a candidate is free to keep its state, so two scripts must never
    collide on a table name".
  - Ticket 13's `fresh_table_name` returns a bare `t{uniq}`, and two
    callers use it:
    - `RENAME TO` targets, which stay in the store;
    - `alter_probe_missing_table`, which assumes `t{uniq}` does not exist.
  - The candidate's store is persistent from ticket 4 on. The next
    verification's generator restarts `uniq` and draws the same small
    numbers.
- **Measured:** the SQLite reference server, one store, 40 stage-13 seeds in
  a row: **3 of the 36 graded runs failed the reference itself.**
  - run 14: `ALTER TABLE t38 aDd CoLUMn c0 INTEGER;` was expected to be
    `ERR no_table` and got `OK 0`. `t38` was renamed in by an earlier run,
    and the probe also changed that stale table.
  - run 35: `ALTER TABLE …_txt RENAME TO t19` was expected to be `OK 0` and
    got `ERR exists`.
  - run 38: `ALTER TABLE t25 ADD COLUMN c0 INTEGER` was expected to be
    `ERR no_table` and got `OK 0`.
- **The rate grows with the store's history:** every repair round and every
  later ticket adds more `t{n}` tables. The shrinker then replays on a fresh
  store, where the mismatch does not reproduce, so the model gets a
  misleading repro.
- **Why the gates missed it:**
  - The 20× reference test (`reference_passes_every_stage_twenty_times`)
    covers stages 0..=11 only.
  - The stage-13 live proof is one run on a fresh store.
- **Fix:**
  - Build fresh table names from the schema's namespace, e.g.
    `s{seed:x}_t{n}`, and keep the uniqueness check.
  - Extend the 20× reference test to `0..=13` on one store, which is how it
    catches this class of bug.

## M1. MEDIUM (ticket 16): the "hard" 64 MiB cap is 64 MiB of RAM plus 64 MiB of swap

- `start_candidate_in_docker_with` (docker.rs:1698) passes
  `--memory=64m` and no `--memory-swap`.
- Docker then allows twice that, and this host has 24 GiB of swap. In
  `subdollar-sandbox` with `--memory=64m`:
  - `memory.max` and `memory.swap.max` are both `67108864`;
  - a Python process that touches 100 MiB prints "alive with 100 MiB
    touched" and exits 0.
- The prompt says "The memory cap is a hard cgroup limit". A server that
  buffers about 100 MiB passes slowly instead of dying.
- **Fix:** when `extras.memory` is set, also pass
  `--memory-swap=<same value>`. Then run the stage-16 live proof again,
  since the reference ran at an RSS of about 17 MB.

## M2. MEDIUM (ticket 15): coverage is self-reported for every non-Go stack

- `run_tests_gate_blocking` takes the number from `coverage.txt`
  (`parse_self_coverage`). `echo "coverage: 99.0%" > coverage.txt` passes
  both the 80 % and 60 % gates.
- Nearly every cell so far is Python, so for those cells the gate is an
  honor system worth 5 points.
- **Options:**
  - measure Python in the harness with the stdlib (`python3 -m trace
    --count --summary`, run around `./test.sh`'s server);
  - or drop the coverage gate and grade sensitivity only, which is the
    signal that matters;
  - or keep it and say in the prompt that the number is not verified.

## M3. MEDIUM (ticket 15): the "revert ticket 7" check reverts much more than ticket 7

- The sensitivity step restores every file that the `stage 7:` commit
  touched to its contents at that commit's parent.
- A typical cell has one server file (`server.py`) that every ticket edits.
  Restoring it to its pre-7 contents also removes tickets 8-14 from it.
- So "./test.sh goes red with ticket 7 reverted" only shows that some test
  covers *some feature newer than ticket 6*. It does not show that the
  aggregate tests catch an aggregate regression. The same applies to
  ticket 8.
- The prompt promises the narrower check ("reverts the ticket-7
  (aggregates) … changes"), and the board will read it that way.
- **Options:**
  - reverse-apply only the ticket's own diff (`git diff parent commit`,
    then `git apply -R --3way` in a temporary clone), falling back to the
    whole-file revert with the fallback logged;
  - or reword the prompt and the log line to what the check actually
    proves.

## M4. MEDIUM (ticket 17): the incident requires 2 MiB statements, which no ticket's contract states

- The failing write is a 2 MiB `INSERT` (`DiskConfig::full().fail_len`),
  and the recovery write is 1 MiB.
- Ticket 0 only says a 2 MiB line must not *kill* the server. A server that
  caps lines at 1 MiB and answers `ERR syntax` (or any other class) is
  within every earlier contract, yet it fails ticket 17 with "expected ERR
  full". The retry also fails, since the 1 MiB write must succeed.
- The size isn't needed: the volume is filled to 0 bytes free, so any write
  already exceeds the free space.
- **Fix:** use about 64 KiB for both writes (the seed size), or state a
  minimum statement size in the prompt.
- **Related, LOW:** the "a further write also answers ERR full" check fails
  an engine that preallocates its journal (a legitimate design that can
  persist a small row). The prompt states it, so this is a contract
  choice; just be aware of it.

## M5. MEDIUM (ops, already flagged by the other agent in TODO_CAMPAIGNS.md): tickets 12-17 can't be reached inside today's cell cap

- The per-ticket budgets for 0-17 add up to **$2.65**, and `max_points` is
  now 56. The cell policy is $0.75 / 120 min.
- The best `.17` cell reached ticket 11 at $0.36 with 13/26.
- The late checks also grow: from ticket 16 on, every verification and
  every repair round runs:
  - `./test.sh` four times (up to 8 min);
  - the 400 MiB load;
  - the perf gate;
  - the crash ladder.
- Five rules bumps in one day, with no live cell in between, moved every
  existing result under "older rules".
- **Operator decision:** a "full ladder" cell class (for example $3-5 / 8-12
  h), or accept the cap stop as the result. Before either, run one capped
  `.23` cell to prove 0-11 still grade the same as on `.17`. This needs
  H1 and H2 fixed first if the cell might reach ticket 13.

## Low

- **L1. Scenario-only stages show `-> PASS` when their check never ran.**
  - The pipeline pushes a placeholder `passed: true` for 12/14/15/16/17.
    The late checks run only if every stage passed.
  - When any earlier stage fails, the verification's stage list still says
    "Stage 12: Concurrency and isolation -> PASS".
  - That misleads the feedback, and it hides a regression of that ticket
    from `detect_regressions`. Use an error text like "not run: an earlier
    stage failed" (and decide whether that counts as a regression), or
    log the placeholder as pending.
- **L2. The ticket-14 connection gauge can race (plausible, not
  reproduced).**
  - `stats_deltas` takes its baseline right after `explain_plans` dropped
    its connection, and after stage 12's dozens of connections.
  - A server that notices EOF late lowers `connections_current` between
    the baseline and the after-read, which gives "moved by -1 across two
    reads on one connection".
  - Only the second-connection close has a settle loop. Settle the gauge
    (two equal reads) before the baseline too.
- **L3. Ticket 14 rejects "INDEX SCAN t.ix".** The "still scans" test is
  any line that contains `scan` and the table name, so the common phrase
  `INDEX SCAN t.ix` fails an indexed plan. The prompt doesn't forbid the
  word. Match a `SCAN <table>` token that isn't part of an `INDEX …` line.
- **L4. Prompt wording.**
  - Ticket 16 says the sort "must spill to disk". `ORDER BY … LIMIT 10` is
    solved by a 10-row heap, which passes, so the sentence is wrong but
    harmless.
  - Ticket 12 says "blocks for more than 5 s fails", but the harness
    allows 10 s per statement.
- **L5. N5 has no length cap.** `render_response` now prints the
  candidate's whole `ERR` text into the feedback and repros. My `rules-18`
  branch capped it at 300 chars. A one-line Python traceback can be long;
  consider a cap.
- **L6. Stage-15 code duplicates the old stage-9 gate.**
  `run_tests_gate_blocking` copies `run_tests_ci_blocking` (stage 9,
  "expiry"). That's fine for now, but the two will drift.

## Suggested order

1. H1 + H2 (ticket 13): map the two SQLite messages, namespace fresh table
   names, assert `unknown` is empty in the soak, and extend the 20×
   reference test to 0..=13. Then run the soak again.
2. M1 (`--memory-swap`) and M4 (the 64 KiB writes): both are one line.
   Then rerun the stage-16 and stage-17 live proofs.
3. M2/M3: decide what ticket 15 grades, and make the prompt say exactly
   that.
4. M5: one capped `.23` cell, then the cap decision.
5. L1-L6 when convenient.

---

# Round 14 (closed)

# Review: round-13 fixes, and the first cell on the fixed argv (2026-10-18, round 14)

Reviewer: Claude Code. Scope:

- `4a7e467` (round-13 fixes: R1, B1-B3, R2);
- `31fbf0d` (opencode cell ops: `.env` key source, provider auth events);
- `3961641` (records the new cell in TODO_CAMPAIGNS.md);
- the DeepSeek cell `campaign_…_20261018_122719` (opencode · effort max ·
  $0.75 / 120 min · rules `.17`, 12:27-13:24);
- the second quarantined cell `poisoned_campaign_…_122342` (the 401 burn).

The stashes were not touched.

**Verdict:**

- **R1 is fixed, and the new cell is valid.** Every attempt ran real turns:
  no `unknown flag`, no `RETRY`, no `FAULT`, no 0-turn round. I reproduced
  the regressions that sank tickets 6, 7, 8 and 11 against the model's own
  code: they are the model's bugs, not the harness's.
- B1, B2 and B3 are fixed. R2 is fixed. R3 is half done (N1).
- New findings are all MEDIUM or LOW (N1-N5). None blocks the next cell.
- **Addendum (2026-10-19, after the `.17` re-runs): G1 is HIGH.** The
  persistence check grades tickets that `.17` says are not required. It
  decided ling's `.17` cell. The DeepSeek `.17` cells are unaffected. Fix
  it before the next cell that can fail a ticket in 1-3.

---

## R1 (fixed): the prompt is never an option

- Both builders end the argv with `--` and then the prompt
  (`sandbox::push_prompt_args`). `omp_task_args` is pure and pinned by a
  test. opencode builds its argv as a `Vec` through the same helper.
- The note now opens with `Harness note (added by the benchmark, not the
  ticket):` and closes with `End of harness note.`
- A source guard forbids `.arg(&effective_prompt)` from coming back.
- **Live check (reviewer, `--network none`, the real sandbox image):**
  - `omp -p --model=foo/bar "--- harness note x"` gives `Error: unknown
    flag`;
  - `omp -p --model=foo/bar -- "--- harness note x"` gives `Model "foo/bar"
    not found`, so the argv parsed.

  This matters because the new cell used opencode, so no omp cell has run
  on the fix yet.
- **0-turn faults:** the round is retried once, then recorded as
  `infra_error` with verification skipped. The repair loop is skipped on
  `infra_error` (pipeline.rs ~4146). See N2 and N3 for the edges.

## The new DeepSeek cell (opencode/max, `.17`): valid

**Result:** reached ticket 5, 6 regression-free, 8/26 points, $0.6077,
9.9/$.

| Ticket | Result | Turns (r0 + r1) | Notes |
|---|---|---|---|
| 0-2, 4, 5 | pass | 29, 19, 13, 19, 25 | |
| 3 | pass in r1 | 13 + 12 | r0: `uPdate<TAB>…` gave `ERR syntax` |
| 6, 7, 8 | fail | 19 + 12 each | regressed 0-5 in both rounds |
| 8 lite | fail | 10 + 9 | r0 6/7 (own stage), r1 regressed |
| 9 | fail | 19 + 12 | 6/7: `INTEGER PRIMARY KEY … UNIQUE NOT NULL DEFAULT` gave `ERR syntax` |
| 10 | skipped | | needs 9 |
| 11 | fail | 25 + 12 | full regressed 0-5, lite 6/7 (`CREATE INDEX` unparsed) |

**Checked:**

- **Spend:** the hand-added opencode catalog entry is `{}`, but the
  `step_finish` events still carry real costs (`spent=0.0335` at s0
  turn 28). So the per-ticket budget saw real spend.
- **Rollback:** the workspace log is stages 0-5, then only the s11 lite
  commit. Each failed ticket was rolled back to `7941450` (stage 5).
- **The regressions are the model's own bugs.** I took `server.py` from
  the s8 r1 commit (`3fd2cbb`) and from stage 5 (`7941450`). I ran both in
  the sandbox image, offline:

  | Build | `CREATE TABLE` | `SELECT a, b FROM t` |
  |---|---|---|
  | stage 5 | `OK 0` | `ROWS 2 …` |
  | s8 r1 | `OK 0` | `ERR syntax internal error: 'list' object has no attribute 'lower'` |

  Every `SELECT` crashes, so the stage-0 suite and the stage-4 sentinel
  fail first. That is a genuine regression.
- The services run the current binary (12:17:59, started 12:23:11).
  `31fbf0d` (12:38) only changed opencode's auth message, which the units
  don't use for scoring.

**Comparability:** this cell is opencode/max. The `.16` DeepSeek cell is
omp/high under older rules. The portal now keeps them in separate rows
(B1), which is correct. TODO_CAMPAIGNS.md's *"one ticket below the omp
`.16` cell"* should say that harness, effort and rules all differ (N5).

---

## New findings

### N1. MEDIUM: R3 is half done; the API still can't flag a campaign

- **Fixed (read path):** `export_sql_board` and `get_campaigns` now skip a
  campaign whose own folder holds `.poisoned` or `.portal_hidden`, with
  tests.
- **Still open (write path):** `set_poisoned` and `set_hidden`
  (portal.rs:67, :82) still require `manifest.json`, which a campaign
  folder doesn't have. So the console can't poison or hide a cell. Both
  quarantined DeepSeek cells are still renamed folders:
  - `…_103729` holds my `.poisoned` note;
  - `…_122342` (the 401 burn) has **no note at all**, so nothing records
    why it is out.

**Fix:**

- Let `set_poisoned` / `set_hidden` accept a campaign folder (has
  `campaign.json`), with a reason.
- Add `POST /api/campaigns/:id/poison|hide`.
- Rename both folders back to `campaign_…` and write a `.poisoned` file
  with its reason: `R1: omp argv crash` and `stale shell key: opencode 401
  at round 0`.
- Test: a poisoned cell flagged through the endpoint leaves both boards
  and comes back when unflagged.

### N2. MEDIUM: a 0-turn harness fault becomes a ladder `fault` and the cell keeps going

`infra_error` maps to `fault` (main_campaign.rs:871, campaign.rs:1858). A
fault gets no lite retry and blocks `needs`, so one harness crash still
removes its dependants from the ladder. The cell then publishes as a
normal measurement. That is the same damage R1 did, now just labelled.

**Fix:** after the retry, a 0-turn `infra_error` should **stop the cell**
with a non-result status (e.g. `harness_fault`, excluded from the boards
like a poisoned cell). Recording it as a fault and moving on is not
enough. Test: a stage whose agent never runs ends the campaign, and the
rollup is not published.

### N3. MEDIUM: "0 turns" is detected from `steps_taken` alone, and only on round 0

The predicate is `partial_stats.steps_taken == 0` on an `Err`
(pipeline.rs ~4031). Three problems:

1. **It misses `OmpBillingError`.** That error carries `partial_stats`,
   but the main path only downcasts `OmpExecutionError`, so a billing trip
   reads as 0 steps. The agent is then **retried against a dead or
   exhausted key** and recorded as a harness fault, not a billing failure.
   Its partial tokens are dropped too. The campaign path has no billing
   trip of its own (only `web/queue.rs` has one), so this label is all the
   operator sees. Fixes:
   - downcast both error types with one helper, as the repair path (~4269)
     already does;
   - never retry a billing error;
   - stop the cell on billing, with the reason. A dead key fails every
     remaining ticket the same way.
2. **It misreads a lost session.** If the session tailer finds no file
   (`final_path` None, omp.rs ~902), stats are default, so a real
   multi-turn run reads as "never ran". It is retried (double spend) and
   then not graded at all. Require `steps_taken == 0 && total_tokens == 0`,
   or better, a positive signal from the harness (`unknown flag` or usage
   text in stderr, exit before the first `step_start`).
3. **It doesn't cover repair rounds.** A repair-round agent that never
   starts is still re-verified and counted as a used round. That is less
   harmful, because the grade reflects the code, but it denies the model
   its repair.

Test each: billing with 0 steps keeps the billing path; tokens > 0 with
steps 0 is graded; a 0-turn repair round is retried.

### N4. LOW: the turn cap counts the killed turn, so the repair round gets one turn less

In ticket 6, `Turn 19 started` and `Finished!` are logged in the same
second against `Max turns: 18`. The recorded turns are 19, and the repair
round then gets `Max turns: 11` (30 − 19) instead of 12. This shows up in
7 rounds of this cell (`turn=19 max_turns=18`, `turn=31 max_turns=30`, …).

**Fix:** count a turn only once it has produced a `step_finish` (or don't
count the turn the cap interrupted). Test: an agent stopped at its cap
records exactly the cap.

### N5. LOW: TODO_CAMPAIGNS.md and feedback wording

- **The TODO says the sentinels *"caught the store drift on rollback"*.**
  The sentinel failed because the candidate's `SELECT` crashed (above),
  not because the store drifted. Replace with: *"the model's
  transaction/aggregate/join changes crashed every SELECT (`'list' object
  has no attribute 'lower'`), so suite 0 and the stage-4 sentinel failed
  first"*.
- **The comparison with `.16`:** add *"different harness, effort and
  rules: not comparable"*.
- **Suggestion (needs a rules bump, so batch it):** feedback shows `Got:
  ERR syntax`, the error kind only. The candidate actually answered `ERR
  syntax internal error: 'list' object has no attribute 'lower'`, which
  names the bug. Showing the candidate's raw answer line in the repro
  would make repair rounds meaningful. Here, all four repair rounds
  repeated the same crash.

### Minor (no action needed)

- `scripts/sql-cell.sh` prints the last four characters of the `.env` key
  when it differs from the shell's key. That is acceptable, but drop it if
  logs are ever shared.
- `ls runs/` still lists two `poisoned_campaign_*` folders. N1 removes
  them.

---

## Suggested order

1. **N3.1** (billing misclassified as a harness fault) and **N2** (stop the
   cell on a harness fault). Both are small, and both keep a harness crash
   from ever reaching a board.
2. **N1** (campaign poison endpoint), then restore and flag both
   quarantined cells.
3. N3.2, N3.3 and N4.
4. N5 text now. The raw-answer feedback goes with the next rules bump.

**Cells:** the R1 blocker is gone. If you want the like-for-like DeepSeek
comparison, run the omp/high cell on `.17` ($0.75 / 120 min). It is also
the first live proof of the omp argv fix.

## Fixes applied by the reviewer (operator-requested, uncommitted)

These were done at the operator's request on top of `3961641` and are
**not committed**.

- **N1:**
  - `portal::set_campaign_flag` writes `.poisoned` / `.portal_hidden`
    with a reason into `runs/<campaign_id>/`. It accepts only
    `campaign_*` folders and only those two flags.
  - New endpoints: `POST /api/campaigns/:id/poison` (`{"poisoned": true,
    "reason": "…"}`) and `POST /api/campaigns/:id/portal` (`{"hidden":
    …}`). They take effect after the units restart.
  - Both quarantined folders are restored to `campaign_…` and flagged with
    reasons. The console shows 8 cells; the portal shows two DeepSeek
    rows (`.17` opencode, `.16` omp) and neither poisoned cell.
- **N2:**
  - `StageOutcome.harness_fault` / `CampaignRollup.harness_fault`: the
    ladder breaks on the first harness fault (no lite, no later ticket),
    and the stop reason is `harness fault`.
  - `campaign run` writes `.poisoned` with the reason before the board
    rebuild.
- **N3:**
  - `OmpExecutionError.started`. For omp: a session file, any stdout, a
    turn, or tokens. For opencode: steps or tokens.
  - `pipeline::agent_error_end` classifies both error envelopes. Billing
    is never retried and sets `harness_fault`.
  - A repair-round agent that never starts is retried once. A second
    failure ends the repair loop without recording the round.
- **N4:** the opencode turn killed at the cap is no longer counted. The
  fake-docker test now expects exactly the cap.
- **N5:** the TODO_CAMPAIGNS.md paragraph is corrected, and a round-14
  fixes paragraph is added. The raw-answer feedback suggestion is
  deferred to the next rules bump.
- **Tests added:**
  - `test_orchestrator_stops_on_harness_fault`;
  - `test_campaign_flag_lifecycle_and_board`;
  - `test_agent_error_end_classifies_billing_starts_and_unknowns`;
  - omp fake-docker `nonzero` (stderr-only exit: `!started`) and
    `nonzero_live` (stdout, then exit: `started`).
- **Fix-run results:**
  - fmt and clippy (`-D warnings`) clean;
  - lib **902 passed**, bins 60, other 2;
  - portal sha256 unchanged by the suite.

## The omp/high re-run on `.17` (reviewer-launched, 14:28-15:11): valid

`campaign_…_20261018_142825`, $0.75 / 120 min.

**Result:** reached ticket 8, **8 regression-free, 13/26 points, $0.3625,
22.1 reg-free/$.**

- Tickets 0-6 and 8 passed. 7 (aggregates) and 9 (constraints) failed 7/8
  and 8/9 on their own stage, with no regressions. 10 was skipped (needs
  9). 11 failed both full (the perf gate: indexed p50 163 ms, against a
  5 ms budget) and lite.
- **The omp argv fix is confirmed live.** Tickets 8, 9 and 11 (full and
  lite) carried the harness note, and every round 0 ran real turns (18,
  18, 24, 9). Ticket 8 passed straight after 7 failed. There was no
  `unknown flag`, no RETRY and no FAULT.
- Not poisoned (no harness fault), and on the portal.
- **Comparisons:**
  - The `.16` omp/high cell (ticket 7, 12/26, $0.7684) used older rules,
    so it is not strictly comparable. Still, this cell went one ticket
    further at half the cost.
  - The opencode/max `.17` cell (8/26, $0.6077) differs only in harness
    and effort.
- The units were restarted at 15:15:39, after the cell finished, and now
  serve the binary with the round-14 fixes.

## The `.17` re-runs (reviewer-launched chain, 2026-10-18 15:23-16:30)

All three ran omp · effort high · $0.75 / 120 min on `526e4bb`. The chain
cost **$0.2886** in total. There was no harness fault, no billing trip
and no `unknown flag`. Every round that carried the harness note ran real
turns (ling's ticket 4: 18 and 12 turns).

| Cell | Earlier rules | `.17` | Cost | Verdict |
|---|---|---|---|---|
| muse-spark-1.3 `_152351` | `.16`: reached 5, 4.8 pts (a live cell with no rollup) | ticket 0 failed, 0 pts | $0.0544 | **Valid.** The 1200 s ticket watchdog fired at turn 15, before a `start.sh` existed, so there was no repair round. Turns averaged about 86 s, against about 48 s on `.16`, where round 0 also took 1212 s but had a candidate by then. |
| ling-3.1-flash `_154400` | `.16`: reached 5, 5.8 pts, $0.7722 | reached 0, 1 pt | $0.2161 | **Not a model result: G1.** Ticket 1 failed on its own (`ERR syntax`, then regressed 0 in r1). Ticket 4 then failed on the grammar of tickets 1 and 2. |
| inkling-small:free `_162652` | 0-1 pts over 4 cells | ticket 0 failed | $0.0181 (effective-cost pricing) | **Valid.** The model's own bug: its server closes the connection on ``SEleCT *<TAB>FROM t``. |

The ticket-4 rollback in ling's cell was correct. Ticket 0's commit
`cb6a285` already held the model's own `load`/`persist` code, which is what
confused the agent ("rolled back to ticket 0, but persistence is there").

## G1. HIGH: the persistence check grades tickets the cell did not pass (a `.17` regression)

`4afb465` (rules `.17`) made the suite *effective*:

- only passed tickets are graded;
- the harness note tells the model that failed and skipped tickets are
  "not graded and not required".

On `.16` the acceptance was the declared ladder, so a fixed profile was
consistent there. Three places still assume the declared ladder:

1. **The dataset.** `scenarios::persistence` (scenarios.rs:81) builds its
   dataset with `sentinel::build(pseed, 4)`, which uses
   `Profile::through(4)`. So the writes use column-list `INSERT` (ticket
   1), and every read is `ORDER BY` … `LIMIT`/`OFFSET`, sometimes with
   `WHERE`/`DISTINCT` (tickets 1-2). Sentinel reads are total-`ORDER BY`
   only, by construction.
2. **The gate.** `driver::run_stage_profiled` runs the scenario when
   `max(acceptance) >= 4`, not when ticket 4 is in the acceptance. Any
   ticket 5-11 whose acceptance excludes 4 is graded on restart
   persistence. Crash (5), transactions (6) and perf (11) are gated
   correctly on `wanted.contains(&N)` (pipeline.rs ~3196/3229/3285).
3. **The upgrade sentinel.** It is gated on `top >= 4` and built with
   `Profile::through(top)` (pipeline.rs ~3406).
   - Seen as a lost check: `_142825` logged `[WARN] sentinel for stage 8
     not written` on `COUNT/SUM/min`, which is ticket 7's grammar, and 7
     had failed.
   - Latent: with 4 not passed, an in-memory server writes the sentinel
     fine, then drifts at the next ticket's restart, giving an
     `[upgrade]` regression of a ticket it passed.

**Seen live:**

- **ling `.17` (`_154400`):** ticket 4's acceptance was `[0, 4]`.
  - r0 failed on `INSERT inTO t (col)<TAB>values ('e3')`, a ticket-1
    column list.
  - r1 failed on `SELECT * … ORDER BY … desc, … Asc LIMIT 6`, which is
    ticket 2.
  - Tickets 5, 6 and 11 hang off 4, so the cell's score is not the
    model's.
- **DeepSeek `_103729`** (already poisoned for R1):
  - ticket 4 (acceptance `[0, 1, 3, 4]`) failed on `ORDER by e0 asc LIMIT
    6 OFFSET 4`;
  - ticket 9 (acceptance `[0, 1, 3, 9]`) failed with `persistence: writing
    the dataset: … ORDER BY`. This is item 2, live.

**Not affected:**

- **DeepSeek `_142825` (omp/high) and `_122719` (opencode/max).** Tickets
  0-6 and 0-5 passed at the full acceptance. `_142825` lost only the
  stage-8 sentinel. That could only make 9 and 11 easier, and both failed
  on their own stage.
- **muse and inkling on `.17`:** they died at ticket 0.
- **`.16` cells:** they graded the declared set by design.

**Fix (suggested):**

- Give the scenario the profile `for_passed(acceptance ∩ 0..=4)`, with the
  lite narrowing. When 0-4 all passed, this is exactly `through(4)`: the
  same seed, a byte-identical dataset, and the same grade for every
  unaffected cell.
- Without ticket 2 there is no total order. Keep unordered reads
  (`SentQuery.ordered`, serde default `true`) and compare them as
  multisets (sort the row lines of the frame).
- Gate the scenario on `acceptance.contains(&4)`, like 5, 6 and 11.
- Write the sentinel only when 4 is accepted, from the same effective
  profile.
- Tests:
  - an `[0, 4]` dataset has no column list, `ORDER BY` or `LIMIT`;
  - `[0, 1, 3, 9]` runs no persistence;
  - a `0..=4` dataset is byte-identical to today's.
- Grades differ only for cells that failed a ticket in 1-3, but an honest
  label is a rules bump (`.18`). It can ship with the deferred N5
  raw-answer feedback.

**Cells:** poison ling `_154400` with reason G1, so it leaves the board.
Re-run ling after the fix.

### G1 fix applied by the reviewer (2026-10-19, operator-requested, uncommitted)

- **What changed:**
  - `driver::persistence_profile(passed, variants)` is tickets 0-4 ∩
    passed, lite-narrowed, and `None` without ticket 4. It feeds
    `scenarios::persistence`, which now takes a profile.
    `run_stage_profiled` runs the scenario only when that profile is
    `Some`.
  - `driver::sentinel_profile` gates the upgrade sentinel on ticket 4 and
    builds it from the suite's own effective profile.
  - `driver::effective_profile` is the one place lite narrowing happens.
  - `sentinel::build(seed, stage, profile)`. With `ORDER BY` in the
    grammar it keeps exactly the ordered reads it always kept. Without it,
    plain reads are kept with `SentQuery.ordered = false` (serde default
    `true`, so old files load as ordered) and compared as a multiset of
    rows. The drift message says "(rows in any order)".
- **Unchanged for full ladders:** with 0-4 passed the profile is exactly
  `Profile::through(4)`, so the dataset is byte-identical. No recorded
  grade of an unpoisoned `.17` cell changes, so the rules stay `.17` and
  the bump waits for N5 and ticket 11. The ticket-4 prompt is unchanged.
  It never named the dataset's grammar, and the multiset compare is only
  more lenient.
- **Ling `_154400` is poisoned** through `POST /api/campaigns/:id/poison`
  (HTTP 200; `.poisoned` names G1). `_103729` keeps its R1 reason.
- **Replay on the live failure:** ling's own ticket-4 image (server.py
  sha256 = commit `1323ab3`) ran in Docker on a scratch store, over 7 seeds
  including both live ones:
  - the old `through(4)` scenario failed every seed (`WHERE`, `DISTINCT`,
    `ORDER BY` reads);
  - the fixed `[0, 4]` scenario and the full `[0, 4]` suite, restart
    included, passed every seed.

  The temporary test was removed afterwards.
- **Tests added:**
  - `driver::persistence_and_sentinel_profiles_follow_the_passed_tickets`
    (the three live acceptances);
  - `sentinel_without_tickets_1_to_3_uses_only_their_grammar`;
  - `sentinel_with_order_by_keeps_only_ordered_reads` (with old-file
    serde);
  - `unordered_reads_compare_rows_as_a_multiset`;
  - `ref_server::reference_suites_run_persistence_only_with_ticket_4`.
    Against the reference, `[0, 1, 3, 9]` never restarts, while `[0, 4]`
    and `0..=4` do and pass.
- **Results:**
  - fmt and clippy (`-D warnings`) clean;
  - lib **907 passed** (5 new), 6 ignored; bins 60; other 2;
  - `portal/index.html` unchanged by the suite.
- Committed as `525c159` at the operator's request. The release binary
  was rebuilt and both units restarted at 09:36:31; both answer 200.

### The ling re-run on `525c159` (2026-10-19 09:36-09:54): valid, ticket 0 failed

`campaign_…_ling-3.1-flash_20261019_093648`, omp · effort high · $0.75 /
120 min.

- **Result:** ticket 0 failed, 0/26 points, $0.0923.
- **Why:**
  - The model spent all 28 turns writing seven `probe*.py` scripts against
    the dev reference, plus `SPEC.md`. It never wrote `server.py` or
    `start.sh`, so verification was skipped ("No runnable start.sh"), and
    with no candidate there was no repair round.
  - The previous `.17` ling cell had written its server by turn 3 and
    passed ticket 0, so this is run-to-run variance, not the harness.
- **Checked:**
  - no harness fault, `unknown flag`, RETRY or FAULT;
  - the probes all got answers, so the reference was up;
  - not poisoned, and on the portal as row 5. The poisoned `_154400` is
    gone from the board.
  - G1 was not exercised live: the cell never reached ticket 4. The replay
    above is the evidence for it.
- **Minor, no action:** the model "discovered" that the reference
  accepts `CREATE TABLE t (a INTEGER,)` while plain sqlite3 rejects it. This
  is true, and the grading oracle does the same:
  - both `translate_ddl`s drop an empty trailing column segment
    (`oracle.rs` `split_top_level`, and the reference's Python twin);
  - so the reference and the oracle agree (`OK 0`);
  - the generator never emits a trailing comma.

  It is a dialect quirk, not a reference/oracle mismatch.

## Test results (HEAD `3961641`, run by the reviewer, niced, no sudo)

- `cargo fmt --check`: clean. `cargo clippy --all-targets -D warnings`:
  clean.
- `cargo test`: lib **899 passed** (8 new), 0 failed, 6 ignored (241 s);
  bins **60 passed**; other 2 passed.
- **Portal isolation is confirmed this time:** `portal/index.html` sha256
  was unchanged across the full suite, with no reviewer rebuild in
  between.
- `runs/`: only `.watch.log` (the timer) changed. There are still 43
  entries.

## The 2026-10-19 chain on `525c159` (rules `.17`, omp/high, $0.75 / 120 min)

Neither cell had a harness fault, an `unknown flag` or a RETRY. Neither is
poisoned, and both are on the portal.

**ling `_102028`: ticket 0 failed, 0/26, $0.1505 (35 turns, 2 rounds).**
Own stage both rounds: `insert INTO t vaLueS<TAB>(…, '19b58<TAB>')`
answered `ERR syntax`, expected `OK 2`. The server does not treat a tab as
whitespace after `VALUES`, or it mishandles a tab inside a string. This is
the model's bug. ling's `.17` record is now:

- `_154400`: poisoned (G1);
- `_093648`: never wrote a server;
- `_102028`: tab handling.

**DeepSeek `_104951`, the second omp/high `.17` sample: reached ticket 5,
5 regression-free, 7/26 points, $0.3149, 32 min.**

| Ticket | Result |
|---|---|
| 0-2 | pass |
| 3 | fail 3/4, own stage: `UPDATE … SET a = COALESCE(50, 5) - (x % x), b = c` gave `ERR syntax` |
| 4 | pass, with acceptance `[0, 1, 2, 4]` |
| 5 | pass |
| 6, 7, 8, 8 lite | fail: the stage-4 sentinel drifted |
| 9, 11 | skipped (need 3) |
| 10 | skipped (needs 7) |

- **G1 was exercised live and works.** Ticket 4 ran with ticket 3 failed.
  The persistence scenario used the effective grammar and passed. The
  sentinel was built from `[0, 1, 2, 4]` (29 writes, 7 ordered reads),
  written, and verified again at ticket 5.
- **The drift is the model's.** The read `select '2g\da' AS e0 FROM
  …_ref42 ORDER BY e0 DESC` expected 19 rows and got 38. Ticket 6's build
  booted on ticket 5's store and doubled every row, most likely by
  replaying the journal on top of the snapshot. This is the upgrade
  contract that ticket 4's prompt states. All four later attempts started
  on the same store, so all four regressed 4.
- **Variance:** the two DeepSeek omp/high `.17` samples are 13/26 and 7/26.
  Ticket 3's parse gap alone cost 9 and 11 (7 points reachable).

### P1. LOW (portal): the "median" of an even number of runs is the upper middle

`portal.rs:1648` uses `sorted[sorted.len() / 2]`, so DeepSeek's ×2 row says
"median 13.0 pts across 2 runs" for 13 and 7; the median is 10.0. Use the
mean of the two middle values for an even count, and add a test with
[7, 13]. This is display only. The board ranks by the best run, as
documented.
