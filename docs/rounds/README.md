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
| [54](54.md) | Tooling: rehearsal cells for $0, seen-failing records, git hooks, citation check (graded: full gate) | merged `7731cda` (operator 2026-10-31 "merge"; R54-1 fixed) |
| [55](55.md) | The TUI shows real numbers: one points scale, live spend, the image field, R52-1; R54-3 (the pre-push hook) | merged `81c1a0b` |
| [56](56.md) | A hung tool call and two lanes: stall guard, the live line names the tool, the audit key delta per lane, `concurrent_cells`, the Board's hung-agent warning, Docker tools refuse beside a cell (R54-2) | merged `9380525` |
| [57](57.md) | Tooling follow-ups: the hook reads its allowlist from the pushed commit (R55-3); a stall is always `wall` (R56-3) | merged `57fbdc3` |
| [58](58.md) | Grading: a busy retry is a time budget with jittered backoff (the bar's stage-12 flake); rules `.45` (full gate) | merged `36c512d` |
| [59](59.md) | Tests: the busy-streak test gets its headroom back (R58-2; graded: full gate, no bump) | merged `f3d4292` |
| [60](60.md) | Tests: the perf self-check measures again once on a loaded host (R59-1; graded: full gate, no bump) | merged `a821adf` |
| [61](61.md) | The bar converges and the gate gets faster: test code leaves the key, one rerun, detached long jobs, a `--ready` gate (graded: full gate, no bump; D1 a, D2 a, D3 yes, D4 `--ready` gate; 9 items) | merged `3da9265` |
| [62](62.md) | Tooling after round 61: a sharper `--ready`, the bar stops on a harness failure, the pre-push hook (R61-6, R61-7, R61-10, R61-11; no graded path) | merged `efb5732` |
| [63](63.md) | The perf port race (R61-12) and the faster bar (R61-8) (graded: `--ready`, no bump) | merged `b8f141e` |
| [64](64.md) | The results page and the Board read clearly: wrong header numbers, the rehearsal on the Board, duration/turns/tokens (R63-2; F6 variance views; no graded path) | merged `5300e8e` |

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
| [65](65.md) | A provider credit failure stops the grid (R63-3); a balance preflight; `publish.sh` uses the one key; fresh input tokens on every route; the published pages read clearly (F6-F8 added after open) (graded: `--ready`, then a bar) | merged `3fae830` |
| [66](66.md) | Ready for the next grid: four new models priced; one board per rules version, every fingerprint shown (graded: `--ready`, then one bar for 65+66) | merged `1a1aa3b` |
| [67](67.md) | The bill matches the snapshot (one pinned provider per model; billed recorded beside list); the GUI shows every run (R66-5, R66-4, R64-3, R66-3) (graded: `--ready`, then a bar) | merged `dc3cacb` |
| [68](68.md) | The bar is required only when grading changes: grader vs plumbing paths, a grader key, a rehearsal record per commit, a pre-grid model check through the sandbox's omp (DECISIONS #26, #28) | merged `9a63909` |
| [69](69.md) | A facelift for the public pages: one palette, a plain header, the board leads with the answer, the results page reads as a report (display only) | merged `6e6c8ee` |
| [70](70.md) | The facelift's follow-ups: the rehearsal footnote, the phone board, cards that match the board, restored tests, polish (R69-2 to R69-7; display only) | merged `5356bd7`, follow-up `2fef700` |
| [71](71.md) | The board stays small and distinct as the grid grows: one agreement strip, a 30-career size budget enforced at publish, 10+ colours with a second channel (R70-2 to R70-4; display only) | merged `aa36801` |
| [72](72.md) | The expanded careers use the board's width: a full-width row, a compact one-line career label, phone cards, equal markers (R71-3; display only) | merged `6459c68` |
| [73](73.md) | Price rows for glm-5.3-flash and ling-3.1-flash (opens after round 68) | superseded by 75 |
| [74](74.md) | Run health: per-phase health numbers, a live check that stops a bad phase, a canary lane, an audit that gates publishing, the `ref-sql` dropouts fixed, health on the GUI | merged `2fa7f5b` |
| [75](75.md) | A new rules version: the reference port fixed in the prompt, omp pinned for real, what the candidate sees graded, pins for 10 models, every model reruns | open |
