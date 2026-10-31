# AGENTS.md: how the agents on this repo work together

subdollarbench measures what budget LLMs can build for under $1. The only
active campaign is **sdb-sql**: a 21-ticket ladder (0-20, 76 points) in 4
career phases. The model builds a SQL server, which is graded by
comparison with SQLite. Specs: `docs/PROPOSAL-sdb-sql.md`. Roadmap:
`TODO_CAMPAIGNS.md`. Work in progress: `docs/rounds/`.

Both coding agents read this file: opencode reads `AGENTS.md`, and
Claude Code reads it through `CLAUDE.md`. When a standing rule changes,
change it here, in the same commit as the work that needs it. Why a rule
exists, and when it was decided, goes in `docs/DECISIONS.md`.

## Roles

| Who | Does | Never does |
|---|---|---|
| **Operator** (the human) | Decides; opens rounds; approves merges, deploys, pushes and paid cells | — |
| **Implementer** (opencode) | Implements a round in its own worktree and branch; ticks the round checklist; runs the gate | Merge to master; restart services; rebuild `target/release` in the main checkout; launch paid cells; push |
| **Reviewer** (Claude Code) | Writes round specs; reviews against the checklist; merges, pushes, deploys and runs cells when the operator says so | Edit code inside an open round's files while the implementer works (see "File ownership") |

## The round workflow

1. **Open.** The reviewer writes `docs/rounds/NN.md` from
   `docs/rounds/TEMPLATE.md` and commits it to master. The operator
   approves; the reviewer tags the commit `round-NN-open`.
2. **Implement.**
   - The implementer branches `roundNN` from the tag, in its own worktree.
   - One item per commit, with the message prefix `RNN <ID>:` (for
     example `R22 F2: a graded crash is failed, not fault`).
   - The implementer ticks each acceptance box and writes its evidence
     under **Implementer notes**.
   - **When a spec item looks wrong** (a definition, a line reference,
     an approach), the implementer asks under **Spec questions** first
     and doesn't implement it as written. The reviewer answers there, on
     master; an answer that changes an item goes under **Changes after
     open**, with the operator's approval.
3. **Frozen once open.** The reviewer never adds items to an open round;
   anything new goes to the next round file (`Status: draft`). Only the
   operator can change an open round, recorded under **Changes after
   open**.
4. **Gate.** The implementer runs `scripts/gate.sh` on the final, clean
   commit (full or `--fast`, see "Gates").
5. **Ready.** The implementer sets `Status: ready <gated commit>` in the
   round file as the last commit and stops. After the gated commit, only
   `docs/rounds/NN.md` may change. The reviewer watches `roundNN` and
   starts the review on its own; the merge still needs the operator.
6. **Review.**
   - The reviewer checks every acceptance box against the code and tests.
   - The reviewer never commits on the implementer's branch. The
     **Review** section goes on master:
     - no blockers: at the merge (step 7);
     - blockers: committed to master without merging. The implementer
       merges master into `roundNN`, fixes them, and sets `ready` again.
   - Every finding has an ID (`RNN-1`, ...) and is either a **blocker**
     (fixed in this round, on the same branch) or **next** (goes to the
     next round file).
7. **Merge.** On the operator's approval the reviewer merges `roundNN`
   (`--no-ff`, no squash), sets `Status: merged <commit>`, tags
   `round-NN-merged`, and pushes (see "Commits").
   - Deploys and paid cells need a separate operator go-ahead.
   - After a deploy, the reviewer checks each item against live data
     (the API, the Board, a real cell). Anything that differs goes in
     the round file and becomes a finding in the next round.

**Round size:** one theme per round (grading, harness, GUI, tooling or
rules), about 5 items at most.

## Operator shortcuts

The operator often can't copy text between terminals, so all
instructions live in the repo and a short phrase is enough:

| The operator says | To | Meaning |
|---|---|---|
| `next round` | Implementer | Read `docs/rounds/README.md` on master. Take the round whose status is `open` (the lowest number if several); if none, say so and stop. Follow its file and this file from step 2, in a new worktree and branch `roundNN` from `round-NN-open`. |
| `blockers` | Implementer | Merge master into `roundNN`, read the round's **Review**, fix every blocker, gate, and set `ready` again. |
| `review` | Reviewer | Review the round whose status is `ready`. |
| `merge` | Reviewer | Merge the reviewed round, record it, push, and draft or open the next one. |
| `gap` | Reviewer | Hold the grid at the next career boundary (the driver's pause file), and say when no cell runs. |
| `resume` | Reviewer | Release the hold; the grid goes on. |

Anything round-specific (a choice, something not to do) goes in the
round file, never only in chat.

## File ownership

| File | Owner | Others |
|---|---|---|
| `AGENTS.md`, `CLAUDE.md`, `docs/DECISIONS.md` | Reviewer, on operator decisions | Implementer proposes changes in its round notes |
| `docs/rounds/NN.md`: header, items, acceptance, review | Reviewer | — |
| `docs/rounds/NN.md`: ticks, **Implementer notes**, `Status: ready` | Implementer | — |
| `docs/rounds/NN.md`: **Spec questions** | Implementer asks | Reviewer answers in place, on master |
| Code, tests, `src/report/rules.rs` history | Implementer during a round | The reviewer's own fixes go in separate commits **after** the merge |
| `TODO_CAMPAIGNS.md` (roadmap only) | Reviewer | — |
| `gui-spec.md` | Operator | Read only |

A round's "done" record lives in its round file, not in
`TODO_CAMPAIGNS.md`.

## Standing rules

- **Paid cells and services:** paid cells (`scripts/sql-cell.sh`,
  `campaign run`) need an explicit operator go-ahead. Only the reviewer
  restarts the services (`subdollarbench-engine`, `subdollarbench-gui`),
  on request.
- **While a cell runs:**
  - Don't touch `target/release` or the services, and change nothing in
    the main checkout but docs. Code work happens in a worktree.
  - Docs-only commits to master are fine: docs enter neither the release
    build nor the graded key. The reviewer checks the key after each one.
  - Only unit tests and `scripts/gate.sh --fast` may run, under
    `nice -n 19`. The full gate, the live proofs, the flake bar and
    sandbox image builds wait for a gap (`gap`) or the end of the grid.
    When a round's next step needs a gap, the implementer says so in its
    notes and stops.
- **Long tests come last** (`docs/DECISIONS.md` #22, #23): the 20-pass
  flake bar, the load bar, the perf baseline and the calibration career
  start only after every planned round has merged.
- **Tests:**
  - Run them niced, and only one `cargo test` on the host at a time
    (tests bind fixed host ports; `SQL_PERF_LIVE` works only inside one
    process). Wrap every test run and gate in the shared lock:
    `flock "$(git rev-parse --git-common-dir)/sdb-gates/cargo-test.lock" nice -n 10 cargo test ...`
    (or `... scripts/gate.sh`). Waiting on the lock is expected.
  - Docker-heavy tests can flake in parallel. Rerun a failure alone
    before calling it real.
  - Tests use workers 8 and above; 0-7 are the live cells' slots
    (`r37_f1_worker_ports_do_not_collide`). A test that runs a campaign
    uses a temp `SDB_RUNS_DIR`, never the real `runs/`.
  - **A new test must be seen failing.** Before `ready`, run each test
    an acceptance box names against the code without its fix (revert
    the fix in the worktree, run the test, restore it). The notes give
    the command and the failing line. A test that can't fail, or can't
    pass, is a blocker.
- **Other people's state:** never drop, pop or apply another agent's
  stash, and never delete another agent's worktree or branch. Don't
  touch Docker volumes or containers that belong to other projects.
- **Killing processes:** by PID only (find it with `ss -ltnp`), never
  `pkill -f` on a pattern.
- **Secrets:**
  - Never print, log or commit values from `.env` (`OPENROUTER_API_KEY`,
    `SDB_UI_*`). Never commit `bench.db*`, `runs/` or `.env`.
  - `~/.omp/logs/http-400-requests/*.json` may hold credentials: read
    fields from them, never paste them.
- **Rules version: frozen** (`docs/DECISIONS.md` #20; the current value
  is `RULES_VERSION` in `src/report/rules.rs`).
  - **A bump** (grading, scoring or what the candidate sees changes)
    needs its own round, with the operator's go-ahead in that round file.
    Its history entry in `src/report/rules.rs` says what changed, why,
    and the score impact. Chained career phases share a rules version,
    so a bump restarts careers at phase 1. Published results always name
    their rules version.
  - **A graded-path change without a bump** needs the full gate and a
    new 20-pass flake bar on that content before the next published cell.
- **Claims cite code:** a statement about behaviour in a round file or a
  review names `path:line`, or the test that shows it.

## Commits

- **Implementer:** `git commit --author="opencode <opencode@localhost>"`;
  message `RNN <ID>: <what>`, then a body that says why.
- **Reviewer:** commits end with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Merges use
  `--no-ff` with the message `Merge roundNN: <theme>`.
- **Both:** one item per commit; never commit to master directly during
  a round. Only the reviewer commits round files and merges on master.
- **Push** (reviewer, after each merge and its record): `master` and the
  new round tags, after both scans come back empty (or every hit is a
  fixture, checked against the real `.env` values without printing
  them):
  - `git diff origin/master..master | grep -iE 'sk-or-|OPENROUTER_API_KEY=.|SDB_UI_(TOKEN|USER|PASS)=.|PRIVATE KEY'`
  - `git ls-files | grep -E '(^|/)\.env$|bench\.db|^runs/'`

## Gates

`scripts/gate.sh` runs on a **clean** checkout: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, and the niced `cargo test`,
in one of two modes:

- **full** (the default): unfiltered, about 45 min;
- **fast** (`--fast`): everything except the tests that hold the
  `SQL_PERF_LIVE` lock (they run one at a time and take nearly all of
  the full gate's time); about 4-10 min. The skip list is printed by
  `scripts/live-proofs.sh`, which reads `src/pipeline.rs`, so a new
  proof is skipped automatically.

It writes `<git-common-dir>/sdb-gates/<commit>.txt` (full) or
`<commit>.fast.txt` (fast). That directory is inside `.git`, shared by
every worktree and never committed.

**Which mode:**

- **In-between commits:** fast, or none.
- **The `ready` commit:** **full** when the round changes a graded path
  since master, **fast** otherwise. The graded paths are listed once, in
  `scripts/graded-paths.txt`; the record's `scope:` line says which mode
  applies. A rules bump always lands in a graded path.
- **The reviewer** reads the gate record for the `ready` commit instead
  of rerunning the suite, reruns what the change puts at risk, and may
  ask for the full gate on any round.

**Flake bar** (`scripts/flake-bar.sh`, `--passes K`):

- It runs the `test_pipeline_eval_only_sql_*_live` proofs K times
  (about 50 min per pass). `flake-bar.sh --key` prints the graded-content
  key; the record is `<git-common-dir>/sdb-gates/flake-<key>.txt`, so
  docs-only commits keep it. A FAIL line names a file with that run's
  output.
- **When:** 20 passes after any graded-path change, before the next
  published cell. `scripts/sql-cell.sh` refuses an official cell without
  a PASS record (`SDB_CELL_SMOKE=1` runs a sub-$0.99 smoke cell without
  one).
- **While it runs:** never start a second runner, and don't run
  `cargo test`, the gate or local Docker builds next to it.

**CI** (`.github/workflows/ci.yml`, on push and pull request): `fmt`
and `clippy` on the pinned toolchain; it builds `subdollar-sandbox` with
the runner's UID and GID; it runs every test except the ones
`scripts/live-proofs.sh` prints, on test workers 8 and above. Line
coverage is gated at 84 % or more, and the ratchet only goes up.

## Pointers

- **Rounds:** `docs/rounds/README.md`. Old reviews (rounds 1-21) and the
  old TODO history: `docs/rounds/archive/`.
- **Decisions:** `docs/DECISIONS.md`. Open questions:
  `docs/OPEN-QUESTIONS.md`.
- **Running a cell:** `scripts/sql-cell.sh <model>`, with `SDB_CELL_PHASE`
  and `SDB_CELL_CHAIN` for career phases.
