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

## Backlog carried over (still relevant to sdb-sql)

- **Scoring and portal:**
  - Pareto frontier of points vs $ (old G4).
  - The campaign portal view: survival curve, cumulative cost, per-stage
    chips, regression badges (old G5).
  - Rank only within (rules version, harness) (old G1).
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
