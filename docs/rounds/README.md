# Rounds

Each unit of work is a **round**: one file `NN.md`, which goes from spec
to checklist, then review, then merge record. The process (who writes
what, branches, gates, freezing) is in `/AGENTS.md`. Start a new round
from `TEMPLATE.md`.

| Round | Theme | Status |
|---|---|---|
| [22](22.md) | Findings from the Haiku career: crash grading, timeout feedback, `waiting` status | merged `91bcb24` |
| [23](23.md) | "What does a dollar buy": dollars, not turns, limit a session | merged `bd7477f` |
| [24](24.md) | The rules fingerprint covers sdb-sql (freeze prerequisite) | merged `fed6519` |
| [25](25.md) | Display follow-ups (crash label, `waiting`, relabel, API cost, refusal text) | merged `b185773` |
| [26](26.md) | Turns inside an attempt follow the dollar rule (R23-7) | merged `644931d` |
| [27](27.md) | Freeze prerequisites: stable stamp, snapshot ids, list price vs bill, flake bar | merged `377b364` |
| [28](28.md) | Freeze hardening: oracle in the stamp, one path list, flake-bar ergonomics, cap shown | merged `2b1db80` |
| [29](29.md) | The freeze: final rules version, CI, last stamp fixes | merged `bde6527` |
| [30](30.md) | CI matches the bench and runs once; status `rules_version`; needle checks print the log | merged `94b24ba` |
| [31](31.md) | A failed live proof keeps its evidence (proof log, flake-bar output, `.41` wording) | merged `1e69d41` |
| [32](32.md) | CI builds the sandbox image (no graded path; gate after the flake bar) | merged `458cf06` |
| [33](33.md) | The CI sandbox runs as the runner's UID (no graded path; gate waived) | merged `f23b3ac` |
| [34](34.md) | The freeze declaration (docs only; rules frozen at `.41`) | merged `9450354` |
| [35](35.md) | CI skips two tests that need the bench host (`ci.yml` only; ratchet 83 on active code) | merged `2eac5aa` |
| [36](36.md) | The candidate's image is stamped; no empty stage-4 dataset (rules `.42`) | merged `cc074f3` |
| [37](37.md) | Tests don't share host state; CI runs the whole fast set | merged `64d83d4` |
| [38](38.md) | A fresh `bench.db` opens on the first try (no graded path) | merged `ba83d58` |
| [39](39.md) | Run is a page from the Board, Live is an overlay (GUI only) | merged `9ec5c32` |
| [40](40.md) | The CLI runs the official cell and says what it ran (no graded path) | folded into 52 (operator 2026-10-30) |
| [41](41.md) | The CLI, the API and the portal serve only sdb-sql (no graded path) | merged `17d09eb` |
| [42](42.md) | `subdollarbench tui`, a terminal console like htop (graded: `Cargo.lock`) | merged `b28674d` |
| [43](43.md) | Delete the retired tasks; the graded findings (graded: full gate + bar) | merged `4e37c4a` |
| [44](44.md) | Publish the grid: careers on the board, the results views, deploy (no graded path) | merged `a3a4e19` |
| [45](45.md) | The perf score: workloads, score, contract, rules `.43` (v2 of the board) | merged `c84c5ea` |
| [46](46.md) | Price the grid models: luna, mimo-v2.6-flash, qwen3.8-flash (graded: `src/cost`) | merged `e44705f` |
| [48](48.md) | Careers in parallel lanes: `--worker`, a grading lock, a load bar, `scripts/grid.sh` (graded: full gate + bar) | merged `1b28eb0` |
| [49](49.md) | W4 counts only durable commits: data on disk, a flush check, R45-3/R45-6, rules `.44` (graded: full gate + bar) | merged `bdffb77` |
| [47](47.md) | The board v2: perf column and tie-break; R44-6/7/8/13 (graded: full gate) | merged `c47e679` |
| [50](50.md) | The grid driver and the flush counter: R48-4..7, R49-2, R49-3 (graded: full gate) | merged `6c04723` |
| [51](51.md) | Publishing: a marked target, one secret list, the results page (R44-9, R44-10, R44-14, Q8 a; R43-1, R43-2; graded: full gate) | merged `0f2e364` |
| [52](52.md) | The operator's tools: GUI, TUI and CLI (R39-4, R42-6..8, round 40; R47-1, R51-1) | merged `0690c3d` |
| [53](53.md) | Two cells at once: a lock per worker, distinct ids, the sweep keeps campaign containers (2-lane smoke; graded: full gate) | merged `e111d69` |
| [54](54.md) | Tooling: rehearsal cells for $0, seen-failing records, git hooks, citation check (graded: full gate) | open (operator 2026-10-31 "1 yes", after 53 merged; before the bar, DECISIONS #23) |

**Status values:**

- `draft`: being written, and may still change.
- `open`: frozen; the implementer works on it.
- `ready <commit>`: implemented and gated.
- `review`: under review.
- `blocked`: review blockers are open.
- `merged <commit>`: done.

Rounds 1-21 predate this process. Their reviews are in
`archive/REVIEW-rounds-01-21.md`, and their specs and "landed" notes are
in `archive/TODO_CAMPAIGNS-to-2026-10-22.md`.
