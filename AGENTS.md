# AGENTS.md: how the agents on this repo work together

subdollarbench measures what budget LLMs can build for under $1. The only
active campaign is **sdb-sql**: a 21-ticket ladder (0-20, 76 points) in 4
career phases. The model builds a SQL server, which is graded by
comparison with SQLite. Specs: `docs/PROPOSAL-sdb-sql.md`. Roadmap:
`TODO_CAMPAIGNS.md`. Work in progress: `docs/rounds/`.

This file is read by both coding agents: opencode reads `AGENTS.md`, and
Claude Code reads it through `CLAUDE.md`. When a standing rule changes,
change it here, in the same commit as the work that needs it.

## Roles

| Who | Does | Never does |
|---|---|---|
| **Operator** (the human) | Decides; opens rounds; approves merges, deploys and paid cells | — |
| **Implementer** (opencode) | Implements a round in its own worktree and branch; ticks the round checklist; runs the gate | Merge to master; restart services; rebuild `target/release` in the main checkout; launch paid cells |
| **Reviewer** (Claude Code) | Writes round specs; reviews against the checklist; merges, deploys and runs cells when the operator says so | Edit code inside an open round's files while the implementer works (see "File ownership") |

## The round workflow

1. **Open.**
   - The reviewer writes `docs/rounds/NN.md` from `docs/rounds/TEMPLATE.md`
     and commits it to master. The operator approves.
   - The reviewer tags the commit `round-NN-open`.
2. **Implement.**
   - The implementer branches `roundNN` from the tag, in its own worktree.
   - One item per commit, with the message prefix `RNN <ID>:` (for
     example `R22 F2: a graded crash is failed, not fault`).
   - The implementer ticks each acceptance box and writes its evidence
     under **Implementer notes** in the round file.
3. **The round is frozen once open.**
   - The reviewer never adds items to an open round. Anything new goes to
     the next round file (`Status: draft`).
   - If something is urgent enough to change an open round, the operator
     says so, and the round file records it under **Changes after
     open**.
4. **Gate.**
   - The implementer runs `scripts/gate.sh` on the final, clean commit.
   - It records the result for that exact commit (see "Gates").
5. **Ready.**
   - The implementer sets `Status: ready <commit>` in the round file
     (as the last commit) and stops.
   - The operator tells the reviewer, or the reviewer notices it, since
     the reviewer may watch `roundNN`.
6. **Review.**
   - The reviewer checks every acceptance box against the code and tests.
   - The reviewer never commits on the implementer's branch, because its
     worktree has it checked out. The **Review** section goes on master:
     - no blockers: right after the merge (step 7);
     - blockers: committed to master without merging. The implementer
       merges master into `roundNN` to read them, fixes them, and sets
       `ready` again.
   - Every finding carries an ID (`RNN-1`, ...) and is either:
     - **blocker**: fixed in this round, by the implementer, on the same
       branch; or
     - **next**: goes to the next round file.
7. **Merge.**
   - The operator approves. The reviewer merges `roundNN` (no
     squash), sets `Status: merged <commit>` and tags `round-NN-merged`.
   - Deploy and paid cells happen only on a separate operator go-ahead.
   - After a deploy, the reviewer checks each item against live data (the
     API, the Board, a real cell), not only against the implementer's
     tests. Anything that differs goes in the round file as a correction
     and becomes a finding in the next round.

**Round size:** one theme per round (grading, harness, GUI or rules),
about 5 items at most. Split anything bigger into the next round.

## File ownership

| File | Owner | Others |
|---|---|---|
| `AGENTS.md`, `CLAUDE.md` | Reviewer, on operator decisions | Implementer proposes changes in its round notes |
| `docs/rounds/NN.md`: header, items, acceptance, review | Reviewer | — |
| `docs/rounds/NN.md`: ticks, **Implementer notes**, `Status: ready` | Implementer | — |
| Code, tests, `src/report/rules.rs` history | Implementer during a round | The reviewer's own fixes go in separate commits **after** the merge, never inside the implementer's open branch |
| `TODO_CAMPAIGNS.md` (roadmap only) | Reviewer | — |
| `gui-spec.md` | Operator | Read only |

A round's "done" record lives in its round file, not in `TODO_CAMPAIGNS.md`.

## Standing rules

- **Paid cells and services:**
  - Paid cells (`scripts/sql-cell.sh`, `campaign run`) need an explicit
    operator go-ahead.
  - The services (`subdollarbench-engine`, `subdollarbench-gui`) are
    restarted only by the reviewer, on request.
- **While a cell runs:** don't touch the main checkout's working tree,
  `target/release` or the services. Work in a worktree.
- **Tests:**
  - Run them niced: `nice -n 10 cargo test`.
  - Docker-heavy tests can flake in parallel. Rerun a failure alone
    before calling it real.
- **Other people's state:** never drop, pop or apply another agent's
  stash, and never delete another agent's worktree or branch.
- **Other projects:** don't touch Docker volumes or containers that
  belong to other projects.
- **Killing processes:** kill only by PID (find it with `ss -ltnp`),
  never with `pkill -f` on a pattern.
- **Secrets:**
  - Never print, log or commit values from `.env` (`OPENROUTER_API_KEY`,
    `SDB_UI_*`).
  - Never commit `bench.db*`, `runs/` or `.env`.
  - `~/.omp/logs/http-400-requests/*.json` may hold credentials: read
    fields from them, never paste them.
- **Rules version (operator decision 2026-10-22):**
  - While sdb-sql is in development, bump `RULES_VERSION` whenever
    grading, scoring or what the candidate sees changes. Bumps are cheap.
  - Every bump gets a history entry in `src/report/rules.rs`: what
    changed, why, and the score impact.
  - Chained career phases must share a rules version, so a bump means
    the next career starts again at phase 1.
  - The freeze comes later, as its own round.
- **Claims cite code:** a statement about behaviour in a round file or a
  review names `path:line`, or the test that shows it.

## Commits

- **Implementer:**
  - `git commit --author="opencode <opencode@localhost>"`.
  - Message: `RNN <ID>: <what>`, then a body that says why.
- **Reviewer:**
  - Commits end with
    `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
  - Merges use `--no-ff` with the message `Merge roundNN: <theme>`.
- **Both:** one item per commit; never commit to master directly during
  a round. Only the reviewer commits round files and merges on master.

## Gates

`scripts/gate.sh` runs, on a **clean** checkout:

- `cargo fmt --check`;
- `cargo clippy --all-targets -- -D warnings`;
- the full niced `cargo test` (unfiltered).

It writes `<git-common-dir>/sdb-gates/<commit>.txt` and prints the path.
That directory lives inside `.git`, so it is shared by every worktree and
never committed.

The reviewer reads the gate file for the `ready` commit instead of
rerunning the whole suite. It reruns only what the change puts at risk.

## Pointers

- **Rounds:** `docs/rounds/README.md`.
- **Old reviews (rounds 1-21) and the old TODO history:**
  `docs/rounds/archive/`.
- **Decisions:** `docs/DECISIONS.md`. Open questions:
  `docs/OPEN-QUESTIONS.md`.
- **Running a cell:** `scripts/sql-cell.sh <model>`, with `SDB_CELL_PHASE`
  and `SDB_CELL_CHAIN` for career phases.
