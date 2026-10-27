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
| [39](39.md) | Run is a page from the Board, Live is an overlay (GUI only) | draft |
| [40](40.md) | The CLI runs the official cell and says what it ran (no graded path) | draft (after the grid) |
| [41](41.md) | The CLI, the API and the portal serve only sdb-sql (no graded path) | draft |
| [42](42.md) | `subdollarbench tui`, a terminal console like htop (graded: `Cargo.lock`) | draft |
| [43](43.md) | Delete the retired tasks; the graded findings (graded: full gate + bar) | draft |
| [44](44.md) | Publish the grid: careers on the board, the results views, deploy (no graded path) | open (re-review: 2 blockers) |

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
