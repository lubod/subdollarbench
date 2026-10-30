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
   - **When a spec item looks wrong** (a definition, a line reference,
     an approach), the implementer asks first, under **Spec questions**
     in the round file, and doesn't implement the item as written. The
     reviewer answers there, on master. An answer that changes an item
     goes under **Changes after open**, with the operator's approval
     (step 3).
3. **The round is frozen once open.**
   - The reviewer never adds items to an open round. Anything new goes to
     the next round file (`Status: draft`).
   - If something is urgent enough to change an open round, the operator
     says so, and the round file records it under **Changes after
     open**.
4. **Gate.**
   - The implementer runs `scripts/gate.sh` on the final, clean commit
     (full or `--fast`, as "Gates" says).
   - It records the result for that exact commit (see "Gates").
5. **Ready.**
   - The implementer sets `Status: ready <commit>` in the round file
     (as the last commit) and stops.
   - `<commit>` is the gated commit. After it, only
     `docs/rounds/NN.md` may change.
   - The reviewer watches `roundNN` and starts the review when it sees
     `ready`, without waiting to be told. The merge still needs the
     operator's approval.
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

## Operator shortcuts

The operator often can't copy text between terminals. All instructions
therefore live in the repo, and a short phrase is enough:

| The operator says | To | Meaning |
|---|---|---|
| `next round` | Implementer | Read `docs/rounds/README.md` on master. Take the round whose status is `open` (with more than one, the lowest number); if there is none, say so and stop. Follow its file and this file, from step 2 of the workflow. Start a new worktree and branch `roundNN` from the tag `round-NN-open`. |
| `blockers` | Implementer | Merge master into `roundNN`, read the round's **Review**, fix every blocker, gate, and set `ready` again. |
| `review` | Reviewer | Review the round whose status is `ready`. |
| `merge` | Reviewer | Merge the reviewed round, record it, and draft or open the next one. |
| `gap` | Reviewer | Hold the grid at the next career boundary (the driver's pause file), and say when no cell runs. |
| `resume` | Reviewer | Release the hold; the grid goes on. |

Anything round-specific (a choice to make, something not to do) goes in
the round file, never only in chat.

## File ownership

| File | Owner | Others |
|---|---|---|
| `AGENTS.md`, `CLAUDE.md` | Reviewer, on operator decisions | Implementer proposes changes in its round notes |
| `docs/rounds/NN.md`: header, items, acceptance, review | Reviewer | — |
| `docs/rounds/NN.md`: ticks, **Implementer notes**, `Status: ready` | Implementer | — |
| `docs/rounds/NN.md`: **Spec questions** | Implementer asks | Reviewer answers in place, on master |
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
- **While a cell runs** (operator, 2026-10-28):
  - Don't touch `target/release` or the services, and change nothing in
    the main checkout but docs. Code work happens in a worktree.
  - Docs-only commits to master (round files, the roadmap, this file)
    are fine: docs enter neither the release build nor the graded key
    (the one `include_str!` of a doc is in a test). The reviewer checks
    the key after each one.
  - Tests: unit tests and `scripts/gate.sh --fast` may run, under
    `nice -n 19`. The full gate, the live proofs, the flake bar and
    sandbox image builds wait for a gap (`gap`, under "Operator
    shortcuts") or the end of the grid. When a round's next step needs
    a gap, the implementer says so in its notes and stops.
- **Long tests come last** (operator, 2026-10-30; `docs/DECISIONS.md`
  #22): the 20-pass flake bar, the load bar, the perf baseline and the
  calibration career start only after every planned round has merged.
- **Tests:**
  - Run them niced: `nice -n 10 cargo test`.
  - Docker-heavy tests can flake in parallel. Rerun a failure alone
    before calling it real.
  - **Two rounds at once** (operator, 2026-10-29: rounds 45 and 48):
    only one `cargo test` runs on the host at a time. Tests bind fixed
    host ports (round 37's port table), and `SQL_PERF_LIVE` works only
    inside one process. Wrap every test run and gate in the shared lock:
    `flock "$(git rev-parse --git-common-dir)/sdb-gates/cargo-test.lock" nice -n 10 cargo test ...`
    (or `... scripts/gate.sh`). Waiting on the lock is expected.
  - **A new test must be seen failing** (rounds 37, 44: tests that could
    never fail, or never pass).
    - Before `ready`, run each test that an acceptance box names once
      against the code without its fix: revert the fix in the worktree,
      run the test, restore the fix.
    - The notes give the command and quote the failing line.
    - A test that can't fail, or can't pass, is a blocker.
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
- **Rules version: frozen at `2026-10-19.42`** (operator decision
  2026-10-26, `docs/DECISIONS.md` #20; the freeze was declared at `.41`,
  and round 36 bumped it):
  - **A bump** (grading, scoring or what the candidate sees changes)
    needs its own round, with the operator's go-ahead in that round file.
    - Its history entry in `src/report/rules.rs` says what changed, why,
      and the score impact.
    - Chained career phases must share a rules version, so a bump means
      the next career starts again at phase 1.
    - Published results always name their rules version.
  - **A graded-path change without a bump** (for example, test code in
    `src/pipeline.rs`) needs the full gate and a new 20-pass flake bar on
    that content before the next published cell (see "Flake bar").
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
- the niced `cargo test`, in one of two modes:
  - **full** (the default): unfiltered, about 40 min (round 31's record
    `3acf7ae264fe.txt`: 2367 s of tests);
  - **fast** (`--fast`): everything except the 15 tests that hold the
    `SQL_PERF_LIVE` lock. They run one at a time and take nearly all of
    the 40 min; the rest takes about 4 min (`b898bcb6a83b.fast.txt`:
    222 s). The skip list is printed by `scripts/live-proofs.sh`, which
    reads `src/pipeline.rs`, so a new proof is skipped automatically.

It writes `<git-common-dir>/sdb-gates/<commit>.txt` (full) or
`<commit>.fast.txt` (fast) and prints the path. That directory lives
inside `.git`, so it is shared by every worktree and never committed.

**Which mode:**

- **In-between commits:** fast, or none.
- **The `ready` commit:**
  - **Full** when the round changes a graded path since master. The
    graded paths are listed once, in `scripts/graded-paths.txt` (the
    verifier, the pipeline, the sandbox, scoring, cost, prompts, the
    rules stamp, `campaigns/`, `extensions/`, `build.rs`, `Cargo.lock`,
    `rust-toolchain.toml`).
    `gate.sh` uses the list for its `scope:` line and
    `scripts/flake-bar.sh` for its record key. A rules bump always lands
    in one of these paths.
  - **Fast** otherwise (GUI, docs, tooling, other report code).
  - The record's `scope:` line says which one applies, so the
    implementer doesn't have to work it out.
- **The reviewer** reads the gate file for the `ready` commit instead of
  rerunning the whole suite. It reruns only what the change puts at risk,
  and may ask for the full gate on any round.

**Flake bar** (`scripts/flake-bar.sh`, `--passes K`):

- It runs the 25 `test_pipeline_eval_only_sql_*_live` proofs K times,
  about 50 min per pass (`31.md`, "After the merge": 20 passes in
  16.6 h).
- The record is `<git-common-dir>/sdb-gates/flake-<key>.txt`. The key
  hashes the graded paths, so docs-only commits keep the record. A FAIL
  line names a file with that run's full output.
- **When:** 20 passes after any graded-path change, before the next
  published cell ("Rules version").
- **While it runs:** never start a second runner (the `SQL_PERF_LIVE`
  lock works only inside one process), and don't run `cargo test`, the
  gate or local Docker builds next to it.

**CI** (`.github/workflows/ci.yml`, on push and pull request):

- `fmt` and `clippy` on the pinned toolchain;
- it builds `subdollar-sandbox` with the runner's UID and GID (round 33);
- it runs every test except the ones `scripts/live-proofs.sh` prints
  (round 37 removed round 35's two bench-host skips);
- test workers 8 and above only: 0-7 are the live cells' slots, and
  `r37_f1_worker_ports_do_not_collide` checks that no two tests share a
  host port (round 37);
- Line coverage is gated at 84 % or more, measured on the active tree
  (round 43 deleted the retired verifiers, so nothing is excluded).
- The ratchet only goes up.

## Pointers

- **Rounds:** `docs/rounds/README.md`.
- **Old reviews (rounds 1-21) and the old TODO history:**
  `docs/rounds/archive/`.
- **Decisions:** `docs/DECISIONS.md`. Open questions:
  `docs/OPEN-QUESTIONS.md`.
- **Running a cell:** `scripts/sql-cell.sh <model>`, with `SDB_CELL_PHASE`
  and `SDB_CELL_CHAIN` for career phases.
