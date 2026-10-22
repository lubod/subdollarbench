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
| [22](docs/rounds/22.md) | Crash grading (F2), timeout feedback (F1), `waiting` status (F3) | open |
| [23](docs/rounds/23.md) | "What does a dollar buy": dollars, not turns, limit a session | draft |
| [24](docs/rounds/24.md) | The rules fingerprint covers sdb-sql; price snapshot | draft |
| 25 (to write) | Freeze: final rules version, flake bar, CI | — |

## After the freeze: validation

- **Calibration** (after round 23): one P1 cell each for Haiku and a much
  cheaper model. Stop reasons should be mostly `dollars`.
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
- **CI:** re-enable GitHub Actions (the workflow exists). The coverage gate
  decision is 50 vs 80 (old D5), and the coverage residuals are in
  pipeline.rs, web/server.rs and web/queue.rs.
- **Decisions log:** one `docs/DECISIONS.md` entry per resolved operator
  decision, including 2026-10-22: dollars bind, API bill only, wall time
  is a guard, rules bumps are free until the freeze.

Items for retired campaigns (sdb-kv redis suites, sdb-edge, sdb-http, HA
for redis: old slices B, C, E, F, I6b, J, L) stay in the archive and are
not planned.
