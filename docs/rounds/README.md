# Rounds

Each unit of work is a **round**: one file `NN.md`, which goes from spec
to checklist, then review, then merge record. The process (who writes
what, branches, gates, freezing) is in `/AGENTS.md`. Start a new round
from `TEMPLATE.md`.

| Round | Theme | Status |
|---|---|---|
| [22](22.md) | Findings from the Haiku career: crash grading, timeout feedback, `waiting` status | merged `91bcb24` |
| [23](23.md) | "What does a dollar buy": dollars, not turns, limit a session | merged `bd7477f` |
| [24](24.md) | The rules fingerprint covers sdb-sql (freeze prerequisite) | open |
| [25](25.md) | Display follow-ups from round 22 (crash label, `waiting`) | draft |
| [26](26.md) | Turns inside an attempt follow the dollar rule (R23-7) | draft |

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
