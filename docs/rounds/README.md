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
| [30](30.md) | CI matches the bench and runs once; status `rules_version`; needle checks print the log | blocked |
| [31](31.md) | A failed live proof keeps its evidence (proof log, flake-bar output, `.41` wording) | merged `1e69d41` |

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
