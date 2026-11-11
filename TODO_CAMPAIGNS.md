# Campaigns roadmap

This file is the roadmap only. The work itself happens in round files
(`docs/rounds/`), following the process in `/AGENTS.md`. The full history
up to 2026-10-22 is in
`docs/rounds/archive/TODO_CAMPAIGNS-to-2026-10-22.md`. Older IDs cited
in code comments (C2, the grid log, M8 and so on) are defined there.

## Where we are (2026-10-22)

- **One active campaign: sdb-sql.** A 21-ticket ladder (0-20, 76
  points). The model builds a SQL server, which is graded by comparison
  with SQLite. Spec: `docs/PROPOSAL-sdb-sql.md`.
- **Career mode:** 4 phases, each its own cell under $0.99. Each phase
  continues the previous one's workspace, data and ladder state; a
  ticket that failed earlier can get a catch-up retry at 0.4x.

  | Phase | Tickets | Max points |
  |---|---|---|
  | 1. Core + durability | 0-6 | 10 |
  | 2. Relational | 7-10 | 12 |
  | 3. Engine | 11-15 | 22 |
  | 4. Hard | 16-20 | 32 |

- **First full career** (rules `.36`, Haiku 5.5, effort high): **39.8/76
  for $2.66**. No harness fault; carry-over, rollback and catch-up all
  worked. It exposed rounds 22-23.

## Next rounds

| Round | Theme | Status |
|---|---|---|
| [22](docs/rounds/22.md) | Crash grading (F2), timeout feedback (F1), `waiting` status (F3) | merged |
| [23](docs/rounds/23.md) | "What does a dollar buy": dollars, not turns, limit a session | merged |
| [24](docs/rounds/24.md) | The rules fingerprint covers sdb-sql; price snapshot | merged |
| [25](docs/rounds/25.md) | Display follow-ups (rounds 22-24) | merged |
| [26](docs/rounds/26.md) | Turns inside an attempt follow the dollar rule (R23-7) | merged |
| [27](docs/rounds/27.md) | Freeze prerequisites: stable stamp (R24-4), snapshot ids (R24-6), list price vs bill (R26-2), missing live proofs, flake bar | merged |
| [28](docs/rounds/28.md) | Freeze hardening: oracle in the stamp (R27-8), one path list (R27-9), flake-bar ergonomics (R27-4), build digest test (R27-5), cap shown (R27-7) | merged |
| [29](docs/rounds/29.md) | The freeze: stamp/label fixes (R28-2..4), CI (D1), final rules version (history names R27-6) | merged |
| [30](docs/rounds/30.md) | CI matches the bench (1.92.0 pin) and runs once; `/api/status` rules version; needle checks print the log; toolchain is graded | merged |
| [31](docs/rounds/31.md) | A failed live proof keeps its evidence (after the stage-18 `infra_error` in the first bar) | merged |
| — | The 20-pass flake bar on 30's merged content: **PASS** on `7f7e15b`, 500/500 (`31.md`, "After the merge"); the first bar on `5ebad6e` failed once (`27.md`) | done |
| [32](docs/rounds/32.md) | CI builds the sandbox image; Node 24 actions | merged |
| [33](docs/rounds/33.md) | The CI sandbox runs as the runner's UID | merged |
| [35](docs/rounds/35.md) | CI skips two tests that need the bench host; ratchet 83 on active code | merged |
| [34](docs/rounds/34.md) | The freeze declaration (AGENTS.md, `docs/DECISIONS.md` #20-21) | merged |
| [36](docs/rounds/36.md) | The candidate's image is stamped; host UID; stage-4 re-seed (rules `.42`) | merged |
| [37](docs/rounds/37.md) | Tests don't share host state; pool readiness; pdf self-skip; CI runs the whole fast set | merged |

## After the freeze: validation

- **Done: the image-stamp round** (round 36, rules `.42`): the image ID
  is stamped and chains need the same image; the image build uses the
  host's UID and GID; an empty stage-4 dataset moves to the next seed.
- **Done: round 37, test infrastructure:** no shared host ports or
  temp paths between tests (with a port-table check), workers 0-7 left
  to live cells, pool readiness, pdf self-skip, CI without round 35's
  skips, and the rollup's image from the first ticket.
- **The fastest route to publishing** (operator, 2026-10-27, "a a pick").
  It replaces "plan a a"'s D1: the cleanup now waits until after
  publishing. Every published cell then shares one fingerprint, the
  current one. Cells after the deletion start a new fingerprint on the
  same rules version.
  - **D2:** a clean calibration career counts as deepseek's first grid
    career. Clean means no harness fault, and stop reasons mostly
    `dollars`.
  - **Pre-approved** (operator, 2026-10-27, "pre-approve all"). The
    reviewer runs these without asking again, each only when its
    conditions hold:
    1. **Merge round 44:** the review has no blockers, the gate is
       PASS, and no cell is running.
    2. **Deploy:** release build and service restart after that merge.
    3. **The calibration career:** the bar says `RESULT: PASS` for the
       current graded content, the tree is clean, and no other cell or
       engine run is active.
    4. **The grid:** the 11 remaining careers, one at a time, rotating
       deepseek → haiku → luna → glm; since 2026-10-28, muse-spark and
       gemini-flash-lite in place of luna and glm (operator: "swap for
       priced models").

    **Stop and wait for the operator** at any of these:
    - a harness fault;
    - a phase over $0.99;
    - a phase that fails for a harness reason (`infra_error`);
    - a new model whose first phase faults;
    - the grid's spend passing $40;
    - a phase in calibration that stops on wall time or turns, not the
      budget cap or the end of its ladder. Finishing a phase's tickets
      under the cap is fine: haiku's archived phase 2 ended at $0.705
      with its ladder done.

    Publishing (`publish.sh --yes`) is **not** pre-approved.
  - **Order:**
    1. **The bar on master's graded content** (`flake-9c90cfcf4b7b.txt`):
       resumed 2026-10-27 at 126/500; **`RESULT: PASS`, 500/500, at
       22:01Z** (`37.md`, "After the merge").
    2. **Calibration:** one deepseek-v4.1-flash-1017 career, then the
       grid, one cell at a time. D3 = (a), changed 2026-10-28 (below):
       deepseek-v4.1-flash-1017, claude-haiku-5.5,
       muse-spark-1.3-contributor and gemini-2.5-flash-lite, 3 careers each
       (about 50 h, $30-35).
    3. **Round 44, merged `a3a4e19` before calibration:** careers on the
       public board, the results page, the variance check, and
       `scripts/publish.sh` to the operator's VPS (D4 = a). No graded or
       fingerprinted file.
       **Calibration done, clean** (2026-10-27T22:15Z to
       2026-10-28T03:33Z): deepseek-v4.1-flash-1017, **70/76 for
       $1.860** (phases 10/10 $0.355, 12/12 $0.489, 22/22 $0.383, 26/32
       $0.633). Stops: `ready` and `dollars`, one `crash` (s14: the model's
       own `kill $(cat /proc/<pid>/stat)` hit its agent; the ticket
       passed). No harness fault, `infra_error`, wall or turns stop. It is
       deepseek's first grid career (D2). The grid follows.
       **Grid career 1, Haiku 5.5:** 66.4/76 for $2.322 (8.8 $0.291,
       12.0 $0.685, 19.6 $0.671, 26.0 $0.676); lite on 5 and 11, s16
       failed. Clean.
       **D3 changed** (operator, 2026-10-28, "swap for priced models"):
       luna (`openai/gpt-6-luna`) and glm (`z-ai/glm-5.3-flash`) are
       not in the graded price snapshot (`src/cost/model_prices.json`),
       and `sql-cell.sh` refused luna's first phase before any API call
       ($0 spent). Adding them would change the fingerprint, and the
       careers already run would need rerunning. Replaced by priced
       models: **`meta/muse-spark-1.3-contributor`** ($0.10/$0.20) and
       **`google/gemini-2.5-flash-lite`** ($0.10/$0.40). The remaining
       10 careers rotate muse-spark → gemini-flash-lite → deepseek →
       haiku. Luna and glm can join with v2 (round 45 bumps the rules
       anyway).
       **Restart** (operator, 2026-10-28, after muse-spark hit 8 h in
       phase 1): "replace" with gpt-6-luna, mimo-v2.6-flash and
       qwen3.8-flash ("use qwen 3.8 flash", in place of 3.6); "prices + 42, then bar"; "5 models". Round 46 adds
       the three prices (graded: the fingerprint changes) and merges
       with round 42; one full gate and one 20-pass bar on the merged
       master; then the grid starts from zero: deepseek, haiku, luna,
       mimo, qwen, 3 careers each (15 careers, ~$35, ~3 days). The `.42`
       careers on the old fingerprint (deepseek 70.0, Haiku 66.4,
       muse-spark phase 1, stopped by the reviewer on "stop it") stay
       unpublished. Tiered prices: open
       question Q8.
       **Stopped again, 2026-10-29** (operator: "ok stop it and fix it"):
       the restarted grid's first cell (deepseek career 1, phase 1) wrote
       Python again, as every model has; stopped at ticket 5 (6/10, about
       $0.17). The `.42` grid is not published. Round 45 (the perf score,
       rules `.43`) goes first; the grid starts again on `.43`.
    4. **Variance check, then publish.**
    5. **v2, the perf score** (operator, 2026-10-28: "double down on
       performance and latency"; publish `.42` first): round 45 (draft),
       rules `.43`, a new bar and a new grid. Round 47 shows it on the
       board.
       Round 48 (draft, operator 2026-10-29 "ok plan it") runs the v2
       grid's careers in parallel lanes: about 28 h for 15 careers with
       3 lanes, instead of about 82 h.
    6. **Opened during the grid** (operator, 2026-10-28: "work on next
       rounds ... without waiting"; the fast gate may run next to a
       cell, AGENTS.md): 39 (GUI) and 41 (CLI and API, plus R38-1)
       merge in career gaps; 42 (`tui`), 43 (deletion) and 45 (perf
       score) are built on their branches and merge after publishing,
       in that order, with one bar for all three.
  - **Round 40: back to draft until after the grid** (round 44 D3). It
    doesn't block calibration: `sql-cell.sh` passes the official values, and the
    reviewer checks the clean tree and the bar by hand before each cell.
  - **Round 38:** merged `ba83d58`.
- **After publishing:** `campaign next <id>` (the round 40 analysis).
  Round 42's `tui` covers the cells list and the ticket table.
- **Calibration and test cells** (after round 23): the test model is
  `openrouter/deepseek/deepseek-v4.1-flash-1017` (operator, 2026-10-22;
  $0.15 / $0.60 per M tokens). Use Haiku only when a second price point is
  needed. Stop reasons should be mostly `dollars`.
- **Grid:** 3-4 cheap models x 3 careers each, on the frozen rules.
- **The bar:** the spread between models must be larger than the spread
  between one model's own careers, and the ranking must hold across
  reruns. If it doesn't, fix variance before publishing.
- **Publish:** points vs dollars (a Pareto view), survival curve
  P(ticket >= i), $/ticket, and regression-free rates.

## The next grid (draft; operator, 2026-11-06)

- **Models (9):**
  - rerun: `deepseek/deepseek-v4.1-flash-1017`, `xiaomi/mimo-v2.6-flash`,
    `anthropic/claude-haiku-5.5`, `openai/gpt-6-luna`, `qwen/qwen3.8-flash`;
  - new: `tencent/hy4`, `minimax/minimax-m3.1-flash`,
    `openai/gpt-6-luna-pro`, `xiaomi/mimo-v2.6-pro`.

  The four new models need entries in `src/cost/model_prices.json`
  (graded, so it goes into a round with a bar).
- **Set aside:** `google/gemini-3.8-flash`, `z-ai/glm-5.3-flashx`.
  Optional wildcards: `inception/mercury-3`, `nvidia/nemotron-3.5-super-vl`,
  `inclusionai/ling-3.1-flash`.
- **No smoke phase for Hy4** (operator). The $0 rehearsal stays.
- **Estimate:** 27 careers, about $68 at list price and about $75 billed;
  about 2.7 days with 3 lanes, about 2 with 4.
- **Decided (operator, 2026-11-06, "go with recommendations"):**
  - **no rerun** of the five v2 models: only the four new models, 12
    careers (about $35 billed, about 1.2 days);
  - one board per **rules version**, with every fingerprint shown
    (round 66 F2);
  - **no Q9 (b)** in this grid;
  - **3 lanes**;
  - a top-up to at least $40 before the grid.

  The plan: round 65 (blockers) → round 66 (prices, the fingerprint
  rule) → one bar → the rehearsal → the grid.
- **One model at a time, cheapest first** (operator, 2026-11-07):
  1. minimax-m3.1-flash;
  2. gpt-6-luna-pro;
  3. mimo-v2.6-pro;
  4. hy4.

  The operator looks at each model's results before the next.
  - Minimax runs off-peak only: its catalog price is 2× during
    01:00-15:00 UTC, which the snapshot's base rate doesn't count.
  - The bar on `589e9fd33cd2` (rounds 65 and 66) passed, 545/545, on
    2026-11-07 at 15:31 UTC.
  - The minimax step (operator: "start minimax") is armed as
    `sdb-grid-arm` (`runs/grid-arm-minimax.sh`): a $0 rehearsal, then
    `runs/grid-v21-minimax.careers` (3 careers), 3 lanes, `--cap 10`,
    and its own ledger `runs/.grid-v21-minimax-ledger.tsv`. The script
    refuses to start inside the 2× hours.
  - **Stopped 2026-11-08** (operator): minimax was billed 1.75× its
    snapshot price, because OpenRouter routes across providers (R66-5).
    Its runs are hidden. Round 67 pins one provider per model; minimax
    reruns after round 67's bar, then luna-pro, mimo-pro and hy4.
  - **Minimax rerun done 2026-11-08** (pinned `gmicloud/fp8`, bar waived,
    DECISIONS #26): median career 11.4 / 76 (1.2, 11.4, 17), last of 6.
    $7.61 at list, $5.64 billed (0.74×). Every phase 1 scored 0 (no
    `go.mod`; ticket 0's $0.09 spent).
  - **Luna-pro started 2026-11-08 17:18 CET** (operator: "start luna"):
    `runs/grid-arm-luna-pro.sh`, a $0 rehearsal, then
    `runs/grid-v21-luna-pro.careers` (3 careers), 3 lanes, `--cap 12`,
    ledger `runs/.grid-v21-luna-pro-ledger.tsv`. The `@openai` pin was
    checked live (served by OpenAI, not `openai/fast` at 2×).
  - **Luna-pro done 2026-11-09 02:48 CET:** median career 58.4 / 76
    (55.8, 58.4, 62.2), #3 of 7. $7.37 at list, $6.82 billed (0.93×).
    Deployed with rounds 69-71; bundle `portal/sdb-results-2026-11-09.zip`.
  - **Mimo-pro started 2026-11-09 07:02 CET** (operator: "start mimo"):
    `runs/grid-arm-mimo-pro.sh`, same shape, ledger
    `runs/.grid-v21-mimo-pro-ledger.tsv`. The `@xiaomi/fp8` pin was
    checked live (served by Xiaomi, bill = snapshot exactly). The balance
    at start was $13.34; at up to $0.99 a phase the grid may pause on the
    balance preflight near the end until a top-up.
  - **Mimo-pro stopped 2026-11-09 08:45 CET** (operator: "stop mimo"):
    two of three phase 1 runs spent ticket 0's $0.09 writing probe
    scripts against the reference server (`probe_ref*.py`) and never a
    server, at 2-5 min per turn (up to 321 s); the third passed ticket 0
    and failed ticket 1. A full grid would have taken 1-1.5 days. Its 5
    paid runs carry `.portal_hidden`; the ledgers and logs stay. Not
    ranked. Round 72 deployed at the stop (bundle
    `portal/sdb-results-2026-11-09.zip`, rebuilt at 08:47 CET).
  - **Hy4 blocked 2026-11-09** (operator: "start hy4"; not started): the
    sandbox's omp (18.8.0, `subdollar-sandbox:latest`) resolves
    `openrouter/tencent/hy4` to **`tencent/hy4-preview`**, a different
    model at 2× the price ($0.834 / $2.501). With the `@tencent/fp4` pin
    the call 404s (the preview has no such endpoint); unpinned, it would
    run the wrong model. The host's omp 18.11.1 resolves it correctly
    once its catalog refreshes (served `tencent/hy4-20261104` by
    Tencent, bill = snapshot), and a `models.yml` entry doesn't change
    18.8.0's choice. The other 8 grid models resolve exactly (checked
    live through omp, 2026-11-09). Options: skip hy4; or a harness round
    that upgrades the sandbox's omp (a new image fingerprint, and the
    question of whether that changes what the candidate sees).
  - **Decided 2026-11-09 (operator: "1"): hy4 is skipped.** This grid
    ends at 7 ranked models (21 careers). The next grid upgrades the
    sandbox's omp under a new rules version and reruns every model
    (`docs/DECISIONS.md` #28).
- **Next models (operator, 2026-11-09):** `z-ai/glm-5.3-flash`, and
  maybe `inclusionai/ling-3.1-flash`, on the current harness (same rules
  as the 7 ranked). Order: round 68 (the bar only for grader changes, the
  pre-grid model check), then round 73 (their pinned rows), then one
  model at a time with a $0 rehearsal.
- **Minimax withdrawn 2026-11-09** (`docs/DECISIONS.md` #29): GMICloud
  broke its tool calls. Board: 6 models; new bundle
  `portal/sdb-results-2026-11-09.zip` (replace the public one). Reruns
  on `minimax` after rounds 68 and 73.
- **Rules bump decided 2026-11-09** (operator, R74-1 → "1"): round 74
  found the prompt gave every model the wrong `ref-sql` port; round 75
  bumps the rules version, pins a current omp, grades what the candidate
  sees, and pins 10 models; then **every model reruns** on the new board
  (about $60-75). **No paid cell on master until round 75 merges.**
- **The `.46` grid** (operator, 2026-11-09): one model at a time, the
  operator looks at each before the next; untested first, cheapest
  first: ling-3.1-flash, minimax (off-peak only), glm-5.3-flash, hy4;
  then luna, deepseek (off-peak only), haiku, luna-pro, mimo-flash,
  qwen. Model 1 (ling) is armed (`runs/grid-arm-v46-ling.sh`): after the
  bar on `c18f72a19608`, a $0 rehearsal, then 3 careers. The model
  checks expire 2026-11-16; re-run them for the models after that.
  - **Ling stopped 2026-11-10 06:36 CET** after 4 phases: one transient
    429 that omp retried tripped the billing fault (round 77 fixes it).
    Scores so far 0-3 points; turns 190-232 s. Resumes on its ledger
    after round 77 and a $0 rehearsal (operator: "1. then resume ling").
  - **The `.46` grid is on hold** (operator, 2026-11-11 "stop ling"):
    before the whole rerun the rules go to `.47`, **Go only** (measured
    coverage and comparable perf for every career; registries off the
    allowlist), and the harness is decided on a small paid comparison
    ("test opencode first"). Order: round 79 (opencode harness made
    safe), the comparison, the `.47` round (one bar), then one rerun of
    every model. Round 78 (language and coverage columns) follows.
    Ling's `.46` phases will be void.
    Ling's grid was killed on the operator's word (2026-11-11 "kill
    ling"): 7 phases in the ledger, 2 cancelled in flight and hidden.
  - **Each model on the operator's approval** (operator, 2026-11-10):
    nothing chains; after each model the reviewer reports and waits for
    "start <model>" (AGENTS.md, "Paid cells and services").
  - **Lanes** (operator, 2026-11-10: "yes"): ling resumes on **1 lane**
    (one small provider; ~15-18 h for its remaining ~9 phases); every
    other model keeps 3 lanes (zero rate limits across 100+ phases on
    big providers). Lane count doesn't change points; each run records
    it. Resume script: `runs/grid-resume-v46-ling.sh`.
  - **Back to 3 lanes** (operator, 2026-11-10 "3 lanes"): on 1 lane
    the remaining 9 phases would take 36-45 h (ling phases run 1-5 h, up
    to the 8 h wall cap); round 77 already makes a 429 a retry. The grid
    holds at the next phase start, then `runs/grid-relane-v46-ling.sh`
    removes career 0's harness-fault row (backup beside the ledger) and
    restarts on 3 lanes.
- ~~Investigate: `ref-sql` drops out~~ (solved by round 74: a prompt
  renderer bug, not an outage).
- (old note) **Investigate: `ref-sql` drops out during agent sessions** (all
  models; 19-27 % of the OpenAI models' reference queries refused, a
  minute later it answers again). Log the reference's health every few
  seconds during a session, find the cause (a crash on some input and a
  restart? a container limit?), and fix it. A round after 68 and 73.
- **Before it starts:** round 65 merged, its bar PASS, and the account
  balance at least the billed estimate plus headroom (round 65 F3 checks
  it per phase).

## Backlog carried over (still relevant to sdb-sql)

- **Scoring and portal:**
  - Pareto frontier of points vs $ (old G4).
  - The campaign portal view: survival curve, cumulative cost, per-stage
    chips, regression badges (old G5).
  - Rank only within (rules version, harness) (old G1).
- **The perf score's load sensitivity (R59-2, `docs/rounds/59.md`):** a
  host-wide burst during a perf run moves a workload's median (the bar's
  W5 0.846 on the reference against itself). In production a lane's perf
  run shares the host with another lane's agent. This needs an operator
  decision on the grader (a load check before and after, or a re-measure
  on a harness note) and a rules bump if it changes the score.
- **The faster bar (R61-8):** now round 63, with the perf port race
  (R61-12), before the grid (operator, 2026-11-03, "1a no bump").
- **The pre-push hook's `++` lines (R62-2, `docs/rounds/62.md`):** the
  pattern check skips an added line whose content starts with `++`.
- **The opencode candidate harness mounts host state** (`docs/DECISIONS.md`
  #25). `src/sandbox/opencode.rs` mounts the host's `~/.config/opencode`
  (plugins, commands) and `~/.local/share/opencode` (every session's
  transcript) into the candidate. Give it a per-run config and data dir
  with only the auth, and a test that a host plugin entry never reaches
  the candidate. Graded (`src/sandbox`): a bar. Until then, cells use
  `omp` only.
- **Failure taxonomy** from campaign stages: truncation, tool misuse,
  regression patterns (old H3).
- **Retirement:** finish the README rewrite for sdb-sql only (old M8,
  "README: campaigns section").
- **Coverage:** the CI ratchet is at 83 %, on active code only (round 35;
  round 29, D1). The residuals are in pipeline.rs, web/server.rs and
  web/queue.rs.

Items for retired campaigns (sdb-kv redis suites, sdb-edge, sdb-http, HA
for redis: old slices B, C, E, F, I6b, J, L) stay in the archive and are
not planned.
