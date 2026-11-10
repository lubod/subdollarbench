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
   commit (fast, `--ready` or full, as its `scope:` line says; see
   "Gates").
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

## The advisor (implementer)

**On** (operator, 2026-11-08): **Gemini 4 Argon, high, on OpenCode
Go** (`opencode-go/gemini-4-argon#high`). It replaced the Opus advisor
on `claude-agent`, which failed every `review` call in rounds 64-66 (the
Claude plan's 5-hour limit, shared with the reviewer). It was chosen
over Kimi K3.1 Pro on a blind review of round 69 (`docs/DECISIONS.md`
#27). Its calls count against the same Go plan as the implementer.
- **It needs an `OpenCode Go` connection.** The plugin checks
  `integration.connection.active("opencode-go")`, and OpenCode keeps
  `opencode-go` as its own integration, apart from `opencode` (the
  Console). `opencode auth list` must show `OpenCode Go`: the Console key,
  registered a second time (`cred_1a1bbb9c021lnJB7H7KrbDxLh`). If the
  Console key is rotated, rotate both.

The implementer (opencode, `deepseek-v4.1-flash`) has an `advisor` tool:
the `@pfoundation/ocadvisor` plugin, configured in the repo's
`opencode.json`, plus `/advisor` (`.opencode/commands/advisor.md`). It
sends the session transcript (the last 200k characters) and returns a
second opinion.
- **Only where the config is.** OpenCode reads `opencode.json` from the
  session's directory and its parents, and a worktree holds only its
  branch's files. A worktree branched before `ed29d7e` has no advisor;
  every `roundNN` from `round-64-open` on does. Config edits reload
  without a service restart.
- **On/off switch:** `disabledForModels` in `opencode.json`. Set it to
  `["opencode-go/deepseek-v4.1-flash"]` for off, or `[]` for on (now).
  It applies on save to sessions in the repo root; a worktree uses its
  branch's copy.

- **When to consult:**
  - `plan` / `before_approach`: before choosing an approach on an item
    that changes a graded path, or that the spec leaves open;
  - `debug` / `stuck`: after a gate or test fails twice for a reason
    that isn't understood;
  - `review` / `pre_complete`: once per round, before the gate run for
    the `ready` commit. Ask it to check every acceptance box against the
    diff and the seen-failing records.
  - Skip it for mechanical edits and docs.
- **The advice never overrides the round file or AGENTS.md.** A
  disagreement with the spec goes under **Spec questions**, and the
  implementer waits for the reviewer's answer.
- **Every consultation is recorded** in the round's **Implementer
  notes**: the mode, the question in one line, the gist of the advice,
  and whether it was followed (with the reason if not). The reviewer
  reads this as part of the review.
- **Never in a candidate's harness.** The plugin lives only in the repo's
  config, never in `~/.config/opencode`. That directory is mounted into
  opencode-harness candidates (`src/sandbox/opencode.rs`). Grid cells
  run `omp` only (`docs/DECISIONS.md` #25).

## File ownership

| File | Owner | Others |
|---|---|---|
| `AGENTS.md`, `CLAUDE.md`, `docs/DECISIONS.md` | Reviewer, on operator decisions | Implementer proposes changes in its round notes |
| `opencode.json`, `.opencode/` (the advisor) | Reviewer, on operator decisions | Implementer proposes changes in its round notes |
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
  `campaign run`) need an explicit operator go-ahead.
  **One model per go-ahead** (operator, 2026-11-10): a grid runs one
  model, and the next model starts only on the operator's word after
  the reviewer has reported the previous one. Never arm or chain a
  second model in advance. Only the reviewer
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
- **Long jobs outlive the session** (`docs/DECISIONS.md` #24): the flake
  bar, the load bar, the grid and multi-hour probes start through
  `scripts/detach.sh <name> -- <command...>`. It runs them as a systemd
  user unit, and linger is on for `ubuntu`. Follow a job with
  `journalctl --user -u sdb-<name> -f`; stop it with `systemctl --user
  stop sdb-<name>` or by its MainPID.
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
  - **A graded-path change without a bump** needs the `--ready` gate
    ("Gates"). A new 20-pass flake bar is needed only when a **grader**
    path changed (round 68, `docs/DECISIONS.md` #26); a **plumbing**
    path needs a $0 rehearsal on the exact commit instead.
- **Claims cite code:** a statement about behaviour in a round file or a
  review names `path:line`, or the test that shows it.

## Commits

- **Author, both agents:** the operator's identity, `lubod
  <lubo.drobny@gmail.com>` (operator, 2026-11-09: "both identities the
  same"). It's set in the repo's git config, which every worktree
  shares, so a plain `git commit` uses it; don't pass `--author`.
  Commits made before 2026-11-09 keep their old authors.
- **Implementer:** message `RNN <ID>: <what>`, then a body that says why.
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
in one of three modes:

- **full** (the default): unfiltered, about 50 min;
- **fast** (`--fast`): everything except the tests that hold the
  `SQL_PERF_LIVE` lock (they run one at a time and take nearly all of
  the full gate's time); about 4-10 min. The skip list is printed by
  `scripts/live-proofs.sh`, which reads `src/pipeline.rs`,
  `src/pipeline/tests.rs` and `src/main.rs`, so a new proof is skipped
  automatically;
- **ready** (`--ready`, round 61): the fast set, then one at a time the
  core proofs (`scripts/ready-proofs.txt`) and the proofs the branch's
  changes put at risk (`scripts/proof-map.txt`, read by
  `scripts/proof-map.sh`). The record names each proof with its reason.
  A change to a shared helper in `src/pipeline/tests.rs` selects every
  proof.

It writes `<git-common-dir>/sdb-gates/<commit>.txt` (full),
`<commit>.ready.txt` or `<commit>.fast.txt`. That directory is inside
`.git`, shared by every worktree and never committed.

**Which mode:**

- **In-between commits:** fast, or none.
- **The `ready` commit:** the mode its record's `scope:` line names:
  - **fast** when no graded path changed since master;
  - **ready** when one did;
  - **full** when `RULES_VERSION` changes (`--ready` refuses a bump).

  The graded paths are listed once, in `scripts/graded-paths.txt`. Its
  `exclude-name tests.rs` and `keep src/pipeline/tests.rs` lines keep
  unit-test files out of the key and the scope, but keep the proofs'
  file in (`scripts/graded-filter.sh`). Test files are also outside the
  rules fingerprint (`src/report/graded_hash.rs`).
- **The live proofs not chosen** still run 20 times in the flake bar
  before the next published cell, when a grader path changed.
- **The reviewer** reads the gate record for the `ready` commit instead
  of rerunning the suite, reruns what the change puts at risk, and may
  ask for the full gate on any round.

**Flake bar** (`scripts/flake-bar.sh`, `--passes K`):

- It runs the bar's 28 proofs (`scripts/bar-proofs.sh`), launched
  through `scripts/detach.sh`. Each proof has a run target
  (`bar_target`): 5 for `r45_f1_reference_scores_one`, 20 for the rest
  (round 63). Since round 63, a stage proof below 20 grades stages 0-4
  and its own; the two stage-20 proofs grade the whole ladder. A full bar
  takes about 7 h.
  `flake-bar.sh --key` prints the full graded-content key (the
  fingerprint and the `--ready` scope). `flake-bar.sh --grader-key`
  hashes only the **grader** paths (round 68), and the bar record is
  `<git-common-dir>/sdb-gates/flake-<grader-key>.txt`, so docs-only and
  plumbing commits keep it. A FAIL line names a file with that run's
  output.
- **Standard** (round 61, D1): a failed proof reruns once right away
  (`TEST <t> RERUN PASS|FAIL`; a rerun doesn't count toward the 20).
  - A failure cured by its rerun is allowed once per proof and twice per
    bar; the result then reads `RESULT: PASS (cured: <proofs>)`.
  - A failed rerun, a second cure on one proof, a third cure in the bar,
    or a FAIL with no rerun after it gives `RESULT: FAIL`.
- **A harness failure stops the bar** (round 62): a run with exit 127,
  `running 0 tests`, or no `test result:` line writes `HARNESS <t>
  <reason>`, and the record reads `RESULT: FAIL (harness: ...)` for
  good. Fix the cause, move the record and its logs aside into
  `sdb-gates/void-flake-<key>-<utc>-<reason>/` (never delete them), and
  restart the bar.
- **Grader or plumbing** (round 68): `scripts/graded-paths.txt` marks
  each graded path. A path is **grader** if the bar's live proofs
  execute it or if it decides a score; the rest are **plumbing**. The
  classes decide only the bar; a change to what the candidate sees still
  needs a rules bump.
- **When:** a full bar (every proof at its target) after any **grader**
  change, before the next published cell. A **plumbing** change needs a
  $0 rehearsal on the exact commit (`grid.sh --rehearsal` writes
  `sdb-gates/rehearsal-<full-key>.txt`). `scripts/sql-cell.sh`,
  `grid.sh` and `publish.sh` refuse an official cell without both
  records (`SDB_CELL_SMOKE=1` runs a sub-$0.99 smoke cell without
  them).
- **Before a model's first grid** (round 68 F3, DECISIONS #28, #29):
  `scripts/model-check.sh <model>` must PASS within 7 days on the current
  sandbox image: one call through the sandbox's omp (the exact model id
  sent and served, the pinned provider, the bill within 1.25×) and a
  tool-call round trip. `grid.sh` refuses a model without it.
- **A provider fault is not a score** (round 68 F5): a phase whose
  provider tool-call errors pass 2 % of turns (at least 5) is marked
  `provider_fault`, hidden, and the grid holds.
- **While it runs:** never start a second runner, and don't run
  `cargo test`, the gate or local Docker builds next to it.

**CI** (`.github/workflows/ci.yml`, on push and pull request): `fmt`
and `clippy` on the pinned toolchain; it builds `subdollar-sandbox` with
the runner's UID and GID; it runs every test except the ones
`scripts/live-proofs.sh` prints, on test workers 8 and above. Line
coverage of the product code (`tests.rs` files excluded, round 68) is
gated at 84 % or more, and the ratchet only goes up. The reviewer
checks CI after every push.

## Pointers

- **Rounds:** `docs/rounds/README.md`. Old reviews (rounds 1-21) and the
  old TODO history: `docs/rounds/archive/`.
- **Decisions:** `docs/DECISIONS.md`. Open questions:
  `docs/OPEN-QUESTIONS.md`.
- **Running a cell:** `scripts/sql-cell.sh <model>`, with `SDB_CELL_PHASE`
  and `SDB_CELL_CHAIN` for career phases.
