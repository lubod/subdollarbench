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

## After the freeze: validation

- **First: the image-stamp round** (`docs/DECISIONS.md` #20, known gaps).
  It also takes the CI-only gaps:
  - stamps the sandbox image ID at run time;
  - drops the host UID 1000 assumption;
  - moves one of the tests that share host port 15380;
  - re-seeds an empty stage-4 dataset (about 1 seed in 5,000; a grading
    change, so it rides on the same rules bump);
  - adds the upstream-pool readiness probe and the pdf test's self-skip,
    then removes round 35's two CI skips.

  Graded, so it needs the full gate and a new 20-pass bar before the
  calibration career.
- **Later: delete the retired campaigns' code** (verifiers, `tasks/`,
  `campaigns/`), keeping old rows readable. Graded, with its own bar.
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
