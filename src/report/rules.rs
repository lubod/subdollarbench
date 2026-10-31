//! Rule-set fingerprint: which prompts + verifier sources graded a run.
//!
//! `RULES_VERSION` is bumped explicitly whenever grading changes
//! (prompt edit, verifier logic, cheat-stage enable). `fingerprint()`
//! hashes the graded inputs so CI fails if content drifts without a bump.
//!
//! The published cell cap is at list price from `src/cost/model_prices.json`;
//! the provider bill is recorded for audit only.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[path = "graded_hash.rs"]
mod graded_hash;

/// Bump this whenever grading changes. History:
/// - `2026-10-19.44`: round 49. W4 counts only durable commits. The perf
///   data dirs live on the runs disk (a memory data root is a harness
///   fault), the harness reads the backing disk's flush-request counter
///   during the 1-client W4 window, and a candidate with fewer flushes
///   than 0.9 x its acknowledged commits floors W4. The ticket-0 contract
///   says what "durable" means. The reference (also the agent's `ref-sql`)
///   waits up to 20 s for a SQLite lock instead of 5 s, so a disk stall
///   in W5 doesn't make it answer `busy` (R49-1; R49-4 named it here).
///   R45-3/R45-6: the career keeps its latest
///   scored perf block, and a scoreless (harness-fault) phase does not
///   replace it. Score impact: points and the cap are unchanged; a server
///   that skips `fsync` no longer gets W4's throughput credit. Careers
///   restart at phase 1; `.43` and `.44` never chain.
/// - `2026-10-19.43`: round 45. The perf score (v2). At the end of every
///   phase the harness measures the final server against the SQLite
///   reference in the same sandbox (2 CPUs, 2 GiB), alternating rounds:
///   W1 point-lookup p50 and W2 range p50 (100k rows), W3 join p50
///   (50k x 5k rows), W4 durable autocommit insert rate (1 and 8 clients)
///   and W5 mixed 80/20 throughput (16 clients, 30 s). Each workload's
///   ratio is clipped to [0.1, 100], a wrong answer floors it, and the
///   phase score is the geometric mean; a career carries its latest phase
///   score. Points stay primary and the perf score breaks points ties.
///   Score impact: the 21 tickets, their points and the $0.99 cap are
///   unchanged; speed is a new, separate axis. The ticket-0 prompt and the
///   phase-opening prompts state the contract, so careers restart at phase
///   1; `.42` and `.43` careers never chain.
/// - `2026-10-19.42`: round 36. Each run records the sandbox image ID it
///   used, and a career phase chains only onto a phase with the same image
///   (D1 a). `update_sandbox_image` passes the bench user's UID/GID, so the
///   image matches the host user (default 1000 is unchanged). Stage-4
///   empty sentinel datasets retry the next seed (up to 16); a seed that
///   built before keeps its exact dataset (the first attempt). Score
///   impact: no change for runs whose stage-4 seed built; roughly 1 in
///   5,000 now gets graded instead of ending as `infra_error`. Careers
///   restart at phase 1 when the image changes.
/// - `2026-10-19.41`: the final version (round 29). The stamp is SHA-256 over a
///   shared manifest, with compiled inputs hashed at build time and
///   `campaigns/sdb-sql/stages`, `campaigns/sdb-sql/campaign.json` and the
///   task prompts hashed from SDB_HOME at run time. New inputs include
///   `extensions/bench-policy.js`, `Cargo.lock`, the linked SQLite version
///   (`rusqlite::version()`, covering `rusqlite`/`libsqlite3-sys`). The
///   price snapshot drops
///   `anthropic/claude-3-haiku`, renames `meta/muse-spark` to
///   `meta/muse-spark-1.3`, and bills Gemini cache writes at the catalog
///   rate: `gemini-3.8-flash` $0.825 -> $0.0416667/M, `gemini-2.5-flash`
///   $0.30 -> $0.0833333/M, `gemini-2.5-flash-lite` $0.10 ->
///   $0.0833333/M. The $0.99 cap is at list price from
///   `src/cost/model_prices.json`; the provider bill is audit-only. Score
///   impact: none for sdb-sql's grading; Gemini cells cost less in cache
///   writes. Careers start again at phase 1.
/// - `2026-10-19.40`: round-26 R23-7 — inside one attempt the only turn
///   bound is the 200-turn guard. The first session gets the whole guard
///   (the retired 50-turn checkpoint is gone). A repair session runs only
///   when the attempt's dollars leave it above `BUDGET_FLOOR_USD`, and
///   gets the guard's remainder (the retired 25-turn cap is gone). The
///   turn rules live in `src/scoring.rs`. Every session's stop reason
///   (initial and
///   each repair) is recorded in the round/stage record, not only the
///   last one. The retired task caps were removed: no test needed them.
///   Score impact: a cheap model's price can buy turns instead of stopping
///   at 50/25; `.39` and earlier careers are not comparable on
///   turn-limited tickets.
/// - `2026-10-19.39`: round-24 freeze prerequisite — the rules stamp now
///   covers the sdb-sql campaign spec, every stage/lite prompt, all of
///   `src/verifier/sql`, `src/scoring.rs` (tier/lite/catch-up weights,
///   `CATCH_UP_SLOTS`, the cap/floor/reserve and the pool weights), the
///   candidate budget/directive prompt files and the committed price
///   snapshot. The retired task inputs stay in the shared stamp: they
///   remain runnable through the generic single-shot path and their
///   verifier/cost code is shared. The snapshot uses exact catalog ids
///   after stripping `openrouter/`; a model variant needs its own entry,
///   and cache-write tokens are billed at the catalog write rate (the
///   prompt rate when the catalog has none). Cost is priced from the
///   snapshot at list price; the live key-usage delta is recorded for
///   audit only. Score impact: none by construction, but `.38` and earlier
///   costs used the live catalog/key delta and are not comparable with
///   `.39` on cost-sensitive tickets.
/// - `2026-10-19.38`: round-23 "what does a dollar buy" — D1: dollars are
///   the session limit; every sdb-sql stage and lite uses a 200-turn
///   runaway guard. D2: a ticket's API budget is the remaining phase
///   headroom after the largest-call reserve, split by weight over the
///   still-undecided tickets; a ticket with a lite weighs
///   `tier * (1 + lite.weight)` so its full attempt leaves the lite its
///   share (R23-1), and a ticket whose need already failed or was skipped
///   weighs nothing; lite and catch-up attempts draw the same pool; the
///   fixed stage/lite budgets are legacy. D3: the cell cap and the pool
///   count the provider API bill (the compute floor is reported only); a
///   paid launch refuses a model with no list price. The recorded cost
///   still prefers the live key-usage delta to the list price (R23-3,
///   round 24).
///   D4: wall time is a guard (480 min/phase, 120 min/ticket). D5: the
///   ticket prompt states its dollars and drops the word-limit/redis
///   wording; the stop reason (dollars/turns/wall/ready/crash) is recorded
///   in the stage record and shown on the Board. Score impact: turn-capped
///   sessions stop being the norm, a cheaper model's price now buys more
///   tickets, and `.36`/`.37` careers are not comparable on turn-limited
///   tickets. The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.37`: round-22 review fixes — F2: a round that was graded
///   before the agent exited non-zero (including the 65,536-token output
///   truncation) is a candidate failure, not a harness fault: it records
///   `failed`, runs the lite, and the stage failure can say `agent_crash`;
///   only a real harness fault (never-started, provider billing/auth, an
///   infra error) stays `fault`, stops the cell and skips the lite. The
///   backfill and git verdict use the same rule. F1: text-protocol and
///   pgwire read timeouts say `no answer within Ns to <statement/step>`
///   instead of a raw errno. F3: a ladder ticket whose unmet need is still
///   pending shows `waiting` (cause `needs N`) until the need is decided,
///   then `skipped`. The `.36` Haiku career is complete and is not
///   comparable with `.37` careers on tickets with a lite. The ladder is
///   unchanged (21 tickets, max_points 76).
/// - `2026-10-19.36` (unchanged, 2026-10-21 round 21): no grading change,
///   so no bump (operator decision 2026-10-21: the `.36` Haiku career
///   continues). The agent's `.37` bump was folded back into `.36`. S3:
///   the console draws a phase cell's ladder as four phase groups
///   (`campaign::phase_ladder`: earlier phases `carried`, later
///   `later_phase`, carried passes count towards `needs`), a career row
///   summarises every phase, and the career phase is `live.career_phase`.
///   Also in this round: the memory-cap live proofs
///   keep their heavy scratch stores on a disk-backed test root (`/var/tmp`
///   by default, `SDB_COVTEST_ROOT` override). The host `/tmp` is a tmpfs;
///   its shmem pages are not reclaimable under the 64/192 MiB cgroup caps,
///   so the old proofs measured the scratch store, not the candidate. The
///   caps are unchanged. S1: the Board keeps harness-faulted cells; a
///   faulted full-ladder cell stays a normal row, a faulted phase cell is
///   an indented `faulted (non-result)` under its career, and careerLeaf /
///   career points still ignore faulted cells. S2: a chain cell named by a
///   non-faulted cell's `refork.of` counts as used (continuing it needs
///   `--refork` in both the CLI and the server pre-check), and a refork
///   names the newest existing successor by `started_at`, not the first
///   `read_dir` hit. The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.36`: round-20 pre-cell — a chain successor counts any
///   running, cancelled, crashed or unfinished cell (only harness-faulted
///   non-results are ignored); the cell writes `chain_from` to status.json
///   at creation; `--refork "<reason>"` records `{of, reason}` and shows a
///   Board badge, with the server refusing an already-used chain until a
///   reason is supplied; the Board's career leaf is the finished cell no
///   finished child names, newest started_at on a refork tie, faulted cells
///   are excluded and running cells hang under the root; a refork's
///   superseded child is labelled; catch-up SPEC.md labels full/lite. The
///   three-slot catch-up decision is recorded as the operator decision of
///   2026-10-21. The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.35`: round-19 review — career catch-ups are greedy over
///   Failed *or* Skipped tickets whose own needs are met, with **three**
///   slots (the operator decision 2026-10-21 for the common 4 -> 5 -> 6
///   chain; supersedes P2's two-slot cap); a catch-up harness fault stops the cell;
///   a catch-up's SPEC.md includes every passed ticket and its acceptance is
///   the declared list plus `passed_all`; the Board ranks a career by
///   `career.points`/spend and expands its phase cells, and the portal keeps
///   phase cells off the public board until its career row exists; a chained
///   cell cannot be continued twice; hints carry each broken ticket's last
///   failure across phases; the pipeline reserves $0.002 for the final
///   verification floor; the shown phase ceiling includes catch-up weight.
///   The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.34`: career phases P2 — a phase start gets at most two
///   catch-up slots for earlier failed tickets needed by phase tickets,
///   ordered by blocked points (lite variant if present, else the full
///   ticket at its budget); a catch-up pass unblocks `needs` but scores
///   0.4 x tier and is recorded `status: "catch-up"`. A phase preamble
///   names each recorded broken ticket with its failure line, repro,
///   blocked phase tickets and points, and its SPEC.md section. The
///   assisted-unblock alternative is assessed and not built.
///   `docs/ASSESSMENT-assisted-unblock.md`. Ladder unchanged (76 points).
/// - `2026-10-19.33`: career phases P1 — the sdb-sql spec splits the ladder
///   into four phases (0-6, 7-10, 11-15, 16-20), validated as a contiguous
///   partition with no forward needs; `campaign run --phase N --chain <id>`
///   continues a phase cell with the previous workspace (including `.git`),
///   store, snapshots, sentinels, baseline and `ladder_state.json`; rollups
///   carry `phase`, `chain_from`, `career` totals and the persisted state.
///   The console launcher has a phase picker and a finished-predecessor
///   chain list; `scripts/sql-cell.sh` accepts `SDB_CELL_PHASE` /
///   `SDB_CELL_CHAIN`. Score impact: cells are phase-scoped, and a career's
///   points are the sum of its phase points (max 76).
/// - `2026-10-19.32`: career-phase P0 cap enforcement — one API budget per
///   attempt (a repair gets only the remainder and does not start below
///   $0.02); the cell cap keeps the largest single call seen so far in
///   reserve, and a ticket starts only when the reserve plus the floor fit;
///   the runners stop in-session at the effective-cost headroom (API plus
///   compute floor), checked in their wait loops; stats carry `cost_usd`
///   and `max_call_usd`, stage outcomes carry `max_call_usd`. Score impact:
///   a ticket whose round 0 spends its API budget is no longer repaired;
///   Haiku 5.5 t9 and DeepSeek `.29` t19 passed only through that repair.
///   The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.31`: M5 — the sdb-sql cell cap is $0.99 / 150 min (from
///   $0.75 / 120): the console launcher, the engine's start default and
///   `scripts/sql-cell.sh` all default to `campaign::CELL_BUDGET_USD` /
///   `CELL_WALL_MIN`. It is the highest budget that keeps a cell under $1.
///   The `.29` calibration cell hit the $0.75 cap at ticket 19 with 49 min
///   left, and a full ladder costs about $1.03 at its rate. The grading
///   code is unchanged, but points compare only within one cap, so the cap
///   gets its own version. The ladder is unchanged (21 tickets, max_points
///   76).
/// - `2026-10-19.30`: round-18 review — ticket 19 lite waits for the
///   replica's LSN to reach the acked commit instead of sampling it once,
///   so an engine that applies a commit just after acking it passes (N1);
///   a failed pause/resume/kill on a node that is still up, or a failed
///   rejoin, is an infra fault (N2); the full scenario requires both
///   replicas to still be replicas after the freeze, and both prompts state
///   the freeze length, that `COMMIT` blocks, the 3 s detection floor and
///   that `lsn` grows only on commits (N3); ticket 12's transfer writers
///   pause 1 ms between transfers so the readers' 10 snapshots overlap them
///   on any engine (N4); late-check skips name a failed late check, the
///   sentinel uses `suite_variants`, and the proposal's ticket-19 ports are
///   current (N5). The failure classifier no longer blames late checks the
///   pipeline did not run ("not run: ..."): a ticket whose own stage failed
///   reads `own_stage`, not "broke tickets 5,6" (N6, seen in the `.29`
///   calibration cell). The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.29`: round-17 review — ticket 19 tests sync commit by
///   freezing the replicas across a `COMMIT` (an answer within 1 s fails;
///   the pending COMMIT must answer after a resume), then requires the
///   lower port to win once both replicas hold the committed LSN; replicas
///   are caught up before the read-only check, so a table-first replica
///   still answers `readonly` (J1/J2). The ticket-15 boot probe allows 25 s
///   to start and 10 s to answer (J3). The PG wrong-password probe bounds
///   message lengths and reports a post-password `AuthenticationOk`
///   correctly; negative tests cover any-password PG auth, the no-ticket-9
///   duplicate path, asynchronous replication and a table-first replica
///   (K1/K2). Late-check skips name the failed stage, the round-16 test
///   comment is restored, and the psql image is a host prerequisite (K3).
///   The ladder is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.28`: round-16 review — ticket 19's post-failover suite keeps
///   every lite variant (I1); the scenario tickets' `needs` now cover the
///   grammar they send (12: 1/2/3/6; 14: 6/11; 16: 2/4/6/7/8/11; 18:
///   1/6/7/8; 20: 1/6/7), 16/17/19/20 use plain DDL and full-table reads,
///   17 sends only ticket-0 grammar, and 20 grades 23505 only when ticket 9
///   passed (I2); the 5 s statement budget is opt-in via
///   `SDB_SQL_STATEMENT_TIMEOUT_MS` (I3); the HA nodes use the worker's own
///   `port+60..62` block and a cluster start failure is an infra fault (I4);
///   ticket 19's first sync-commit attempt killed the primary immediately
///   after the COMMIT ack and derived the election from post-kill LSNs; that
///   could not distinguish an asynchronous ack and is superseded by `.29`
///   (I5); the PG wrong-password probe and psql image requirement, T18 scope wording, skipped-stage
///   regression accounting and the T14 settle pause (L-b..L-e). The ladder
///   is unchanged (21 tickets, max_points 76).
/// - `2026-10-19.27`: ticket 20 (PostgreSQL wire protocol) — a scenario-only
///   stage with a simple-query lite variant: protocol 3.0 on `--pg-port`
///   (SSL refusal, startup/cleartext auth, ParameterStatus, simple and
///   extended query, `I`/`T`/`E`, SQLSTATE 42P01/42601/23505) plus a stock
///   psql cross-check. The ladder is 21 tickets (max_points 69 -> 76).
/// - `2026-10-19.26`: ticket 19 (replication and failover) — a scenario-only
///   stage with a two-node lite variant: three nodes (or two) launched with
///   `--replicaof` and `SDB_SQL_NODES`, `SHOW ROLE`/LSN, read-only replicas,
///   sync-acked commits, automatic election (highest LSN, lowest-port tie)
///   or manual `PROMOTE`, rejoin, and the cumulative differential suite
///   against the current primary. The ladder is 20 tickets (max_points
///   62 -> 69).
/// - `2026-10-19.25`: ticket 18 (hostile input and limits) — a scenario-only
///   stage: the candidate is restarted with `SDB_SQL_PASSWORD` on a fresh
///   scratch store; the harness checks AUTH (and that the password never
///   reaches an error or a log), byte fuzzing and grammar mutations, a
///   10,000-deep expression, a statement over 16 MiB, a 5 s statement
///   cancellation, a slow-loris client, and the 256-connection cap. The
///   candidate container gets room for 256 connection threads. The ladder
///   is 19 tickets (max_points 56 -> 62).
/// - `2026-10-19.24`: round-15 review — ticket 13 maps SQLite's PRIMARY
///   KEY/UNIQUE drop refusals to `constraint` and namespaces rename targets
///   and missing-table probes (H1/H2); ticket 16's memory cap also disables
///   swap (M1); ticket 15 discloses that non-Go coverage is self-reported
///   and reverses only the ticket's own diff when it still applies, logging
///   the whole-file fallback (M2/M3); ticket 17 uses 64 KiB writes and no
///   longer requires a second write while full (M4); a scenario-only stage
///   that never ran says "not run" instead of PASS (L1); "INDEX SCAN" is an
///   index plan (L3); candidate error text is capped at 300 chars (L5).
///   No ladder change (18 tickets, max_points 56).
/// - `2026-10-19.23`: ticket 17 (incident: disk full) — a scenario-only stage:
///   the candidate runs on a fresh 16 MiB tmpfs, the harness fills it with a
///   foreign filler, a too-large write must answer `ERR full` without being
///   acked or applied, the acked seeds must read back exactly, and the same
///   write must succeed after the filler is removed (no restart). The ladder
///   is 18 tickets (max_points 50 -> 56).
/// - `2026-10-19.22`: ticket 16 (larger than memory) — a scenario-only stage:
///   the candidate is restarted on a fresh scratch store under a 64 MiB
///   cgroup cap; the harness loads 400,000 rows with ~900-byte text cells
///   (~400 MiB on disk), then checks a full-table aggregate, an external
///   `ORDER BY ... LIMIT 10` and an indexed join exactly, with the server
///   alive throughout. The ladder is 17 tickets (max_points 44 -> 50).
/// - `2026-10-19.21`: ticket 15 (tests and CI gate) — a scenario-only stage
///   with a lite variant (coverage 60% vs 80%): the candidate ships an
///   offline `./test.sh` that must pass twice in under 120 s per run, a
///   coverage signal (Go measured, otherwise a `coverage: NN.N%` line in
///   coverage.txt), and tests that go red when the ticket-7 (aggregates) or
///   ticket-8 (joins) commit is reverted in a scratch copy (sensitivity).
///   The ladder is 16 tickets (max_points 39 -> 44).
/// - `2026-10-19.20`: ticket 14 (observability) — a scenario-only stage
///   (no new grammar): `EXPLAIN <select>` answers a plan that names
///   `INDEX <table>.<index>` for an indexed equality and `SCAN <table>`
///   without one, and `SHOW STATS` answers the ten counters
///   (statements/selects/inserts/updates/deletes/commits/rollbacks/errors/
///   connections_current/connections_total) whose deltas after scripted
///   traffic are exact: statements counts every line (the SHOW itself
///   included), statement kinds are counted by first keyword even on
///   failure, errors counts ERR answers, and the connection gauge counts
///   open/accepted connections. The ladder is 15 tickets (max_points
///   34 -> 39).
/// - `2026-10-19.19`: ticket 13 (schema change) — differential grammar:
///   `ALTER TABLE ADD [COLUMN] [IF NOT EXISTS] name TYPE [DEFAULT lit]`
///   (existing rows read the default), `DROP [COLUMN] [IF EXISTS]`
///   (PK/UNIQUE/indexed refused as `constraint`, missing column `no_column`,
///   a missing name under `IF EXISTS` is OK 0), `RENAME COLUMN old TO new`
///   and `RENAME TO new` (taken names are `exists`, unknown names
///   `no_column`/`no_table`); SQLite has no ALTER `IF [NOT] EXISTS`, so the
///   oracle and the reference emulate the existence check from the live
///   schema. Indexes and foreign keys follow renames; every change survives
///   restart. The ladder is 14 tickets (max_points 30 -> 34).
/// - `2026-10-19.18`: ticket 12 (concurrency and isolation) — a
///   scenario-only stage (no new grammar): the lost-update check
///   (16 clients x 50 read-modify-write transactions must end at exactly
///   800), the transfer invariant with snapshot readers (the sum never
///   changes), and the full-only reader check (a reader answers the
///   pre-transaction value within 1 s while a writer is open; neither
///   blocking nor `ERR busy` passes; the lite variant
///   `concurrency-global-lock` skips it). The ladder now has 13 tickets
///   (max_points 26 -> 30). Feedback also carries the candidate's raw error
///   text ("ERR syntax internal error: …"), not just the class, so a repair
///   round sees the bug (round-14 N5).
/// - `2026-10-18.17`: round-10 harness fixes — the per-round
///   `FEEDBACK_REPORT.md` is deleted before and after every attempt and
///   never committed (a stale report used to greet later tickets and be
///   believed); a failed or skipped ticket no longer reads as contract
///   (`SPEC.md` collapses its section with a FAILED/SKIPPED marker and the
///   report is regenerated from the ladder's outcomes), every attempt
///   after a failure opens with a harness note naming the broken tickets
///   and the *effective* acceptance suite the verifier runs, and an
///   attempt's budget is clamped to the cap's remaining headroom (the
///   ladder stops as "budget cap" below a $0.02 floor; the clamp converts
///   the cap's *effective* headroom into the API dollars OMP enforces,
///   using the cell's observed api/effective ratio — 0.25 before the first
///   attempt, which is *deliberately* conservative: a $0.75 campaign's
///   ticket 0 starts with $0.1875 rather than its $0.20 ticket budget,
///   because the cap's effective ceiling binds — so a 3.4x floor can no
///   longer turn a $0.05 headroom into a $0.17 effective overshoot
///   (round-11 findings 1-2), and the four
///   remaining prompt openers name a ticket as contract rather than claim
///   it passed (round-11 finding 3).
/// - `2026-10-17.16`: the perf gate compares *net work* over the
///   round-trip floor (indexed work <= scan work / 10, or <= 0.05 ms
///   absolute), so a constant per-statement overhead cannot lift the
///   floor and hide a missing index.
/// - `2026-10-17.15`: round-6 grading fixes — subquery inner tables are
///   never the outer statement's (round-4 finding 3 actually lands; a
///   same-table subquery binds every correlated name to the inner scope),
///   `CREATE INDEX` / `DROP INDEX` may appear inside a transaction and a
///   rolled-back index is gone for the generator too, the perf gate runs
///   on a scratch store, restarts gracefully (ticket 4's contract, not
///   SIGKILL) and accepts an index at the round-trip floor (<= 2x an
///   empty query) without the 10x ratio, and campaign rollups carry the
///   rule set they were graded under.
/// - `2026-10-17.14`: sdb-sql ticket 11 (indexes + perf gate) —
///   `CREATE [UNIQUE] INDEX` / `DROP INDEX` enter the graded DDL with the
///   schema-object error classes, a UNIQUE index constrains like the
///   column form, indexes change no answer and survive restart, and the
///   perf gate (200k rows, 500 point lookups, p50 < 5 ms and >= 10x,
///   ranges full-only, a restart in the middle) enters grading. (Its
///   message also claimed round-4 finding 3; the code did not have it
///   until `.15` — the entries below are what actually graded.)
/// - `2026-10-17.13`: round-4/5 grading fixes — SIGTERM reaches PID 1 and
///   every other process, every crash-ladder (b) attempt is pinned and
///   (c)/(d) must not change the state they saw, the isolation peer's
///   *class* decides retryability, `INSERT ... SELECT` keeps its user
///   predicate parenthesized and its INTEGER key below 2^62 (SQLite picks
///   random rowids at i64::MAX), and `INTEGER PRIMARY KEY` auto-assign is
///   `max + 1` even for negative maxima.
/// - `2026-10-16.12`: sdb-sql ticket 10 (subqueries) — scalar
///   subqueries (aggregate-wrapped, one column), `[NOT] IN (subquery)`,
///   `IN (list)` with three-valued NULL semantics, `[NOT] EXISTS`,
///   correlated references and `INSERT ... SELECT` enter the graded
///   contract.
/// - `2026-10-16.11`: statement atomicity is graded the same way in
///   autocommit and inside a transaction (the oracle wraps statements in a
///   savepoint; the reference does the same), a clean stop delivers
///   SIGTERM to every process in the container, the suite is the *passed*
///   stages plus the current ticket, `ORDER BY`/`LIMIT` in aggregate and
///   join queries are gated on ticket 2, and the crash probes read the
///   socket after the kill (an answered statement is owed).
/// - `2026-10-16.10`: sdb-sql ticket 9 (constraints) — primary keys
///   (INTEGER auto-assign, TEXT NOT NULL), `UNIQUE`, `NOT NULL`,
///   `DEFAULT` and `REFERENCES` (restrict, `foreign_keys=ON`) enter the
///   graded DDL and error classes.
/// - `2026-10-16.9`: sdb-sql ticket 8 (joins) — comma/`INNER`/`LEFT
///   [OUTER] JOIN` over 2-3 items, `ON` predicates, qualified and unique
///   bare references, `ERR ambiguous`, `SELECT *` in FROM order; the lite
///   variant keeps comma/inner joins (a lite pass narrows the profile of
///   every later ticket).
/// - `2026-10-16.8`: sdb-sql ticket 7 (aggregates) — `COUNT`/`SUM`/`MIN`/
///   `MAX`, `GROUP BY`, `HAVING`, the empty-group contract and the
///   aggregate error classes enter the graded contract; the multi-row
///   statement-atomicity probe is now autocommit-only.
/// - `2026-10-16.7`: http stages 9/10 — a TCP accept after the 128MB-capped
///   restart is not readiness (the pipeline now waits up to 20s for an
///   actual HTTP answer before grading, as the pre-verification setup
///   already did), and the hardening stage's health probes no longer reuse
///   pooled keep-alive connections, which the candidate's own idle reclaim
///   could close between probes. Timing only: a candidate that never serves
///   still fails. The sdb-sql campaign also gained its real launch contract
///   (`--port`/`--data`, a harness-owned data directory) and an
///   embedded-SQL scanner that covers JVM/.NET/PHP and no longer flags
///   inline prose; that task is not on the board yet.
/// - `2026-10-15.6`: operator decisions — (1) the proxy prompts now note
///   that the container's `http_proxy`/`https_proxy` env vars make tools
///   like curl route through an egress proxy (raw sockets are unaffected;
///   pass `--noproxy '*'` when probing `upstreamN`), removing a turn tax
///   unrelated to the measured skill; (2) a **readiness grace** for the
///   proxy and http tasks: after the port accepts, the pipeline probes
///   `GET /` for up to 20s and only then starts grading, so a candidate
///   that binds early but serves late (slow startup self-test) is not
///   failed by the verifier's single first request. The captured
///   transcript runs are unaffected (they pre-date both).
/// - `2026-10-15.5`: sdb-http hardening tolerance — a garbage request line
///   ("GARBAGE / HTTP/1.1") is accepted when rejected with ANY 4xx
///   (400/405/501/505) or a clean close, not only 400/505 (an unknown
///   method is a method error, not necessarily a syntax error). The
///   hardening prompt says the same. No other task is affected.
/// - `2026-10-15.4`: campaign sdb-http (task `http`, tickets 0-7). The
///   single-shot http suite (stages 1-5: root/404/echo/user-agent/files)
///   is unchanged; five campaign-only stages extend it: (6) byte ranges
///   (206 + `Content-Range`, suffix/open ranges, 416 unsatisfiable,
///   plain GET unchanged), (7) conditional GET (`ETag` +
///   `Last-Modified` validators, `If-None-Match`/`If-Modified-Since` ->
///   304 with no body, stale validators never 304, rewritten content
///   changes the ETag), (8) gzip negotiation (`Accept-Encoding: gzip` ->
///   `Content-Encoding: gzip` with a decodable, smaller body; identity ->
///   raw bytes), (9) chunked uploads + `Expect: 100-continue` (no
///   deadlock) + a declared 128MiB body refused (`413`) under a **128MB
///   cap** restart, (10) hardening (path traversal in three spellings
///   never leaks; 32KiB heads refused; malformed request lines 400;
///   slow-loris reclaimed <=12s) under the same cap. Campaign acceptance
///   for task `http` validates ids 1..=10 (`http_stages`).
/// - `2026-10-15.3`: campaign sdb-queue (task `queue`, tickets 0-8) — a new
///   task type (default port 6385) with a 9-stage suite: (1) ENQ/LEASE/ACK
///   FIFO round trip, (2) visibility leases + expiry + connection ownership,
///   (3) NACK immediate/delayed + attempt accounting, (4) dead-lettering
///   after 3 failed attempts into `<queue>.dead` (inspections do not
///   increment attempts), (5) durability across kill -9 (acked never
///   return; in-flight jobs return with attempts preserved), (6) no
///   duplicate delivery under 6 racing consumers, (7) per-queue STATS
///   counters, (8) delayed ENQ + DLQ REDRIVE, (9) hardening (32KiB payload
///   rejected, malformed lines error, slow-loris reclaimed, 128MB cap).
///   Campaign acceptance for task `queue` validates ids 1..=9
///   (`queue_stages`); other tasks are unaffected.
/// - `2026-10-15.2`: campaign sdb-edge (task `proxy`, tickets 0-6). The
///   single-shot proxy suite (stages 1-5) is unchanged; four campaign-only
///   stages extend it: (6) fast failover under a hung upstream (frozen
///   process, TCP still accepting — every request 200 inside 2500ms,
///   rejoin <=10s), (7) active health checks + `GET /status`
///   (`upstreamN: up|down` lines, flips while idle, recovery <=5s),
///   (8) caching with `SDB_PROXY_CACHE_TTL` (X-Cache MISS/HIT + Age,
///   byte-identical bodies, expiry; the pipeline restarts the candidate
///   with the env for this stage), (9) limits under a 128MB cap (32KiB of
///   headers -> 4xx/close, declared 100MB body -> 413/close before the
///   bytes, slow-loris reclaimed <=12s, liveness after). Campaign
///   acceptance for task `proxy` validates ids 1..=9; campaign.rs gained
///   per-task acceptance lists (`proxy_stages`). Single-shot proxy rows
///   are unaffected; campaign rows are new-stamp only.
/// - `2026-10-15.1`: campaign sdb-kv stage-2 wave (tickets 11-13). New
///   pipeline-level stages 14-16: (14) disk-resident store under a 192MB
///   memory cap on a host-bind data dir (~480MB written, SIGKILL +
///   restart re-read), (15) compaction (churn ~860MB, 48MiB live; the
///   harness measures the host bind mount and requires <= 3x live bytes
///   within 30s, survivors byte-exact through a restart), (16) admin
///   binary protocol SDB1 v2 on the worker's 6395 port (framing fidelity
///   incl. checksum/unknown-opcode/pipeline, shared keyspace with RESP in
///   both directions, STATS mirroring INFO fields). The candidate
///   container publishes 6395 only when a ticket's acceptance includes 16;
///   CandidateRunExtras gained host bind mounts. Runs only when a
///   ticket's acceptance explicitly includes 14/15/16; acceptance ids up
///   to 16. Rows stamped `2026-10-14.13` or earlier are not comparable
///   without `backfill --yes`.
/// - `2026-10-14.13`: anti-cheat proxy rule refined. The scanner flagged
///   `nc`/`socat` in ANY mode, so a candidate that piped a RESP payload
///   into `nc -q1 127.0.0.1 6379` inside its own TEST script was
///   disqualified (live run 11 false positive). Only LISTEN/EXEC-mode
///   invocations are the cheat vector (serving via a tunnel to the
///   official reference, or exec'ing a shell) and those still fail with a
///   message naming listen/exec mode; client-mode use is allowed. Rows
///   stamped `2026-10-14.12` or earlier are not comparable without
///   `backfill --yes`.
/// - `2026-10-14.12`: campaign ticket 10 (three-node HA finale, operator
///   decision Q1). New pipeline-level stage 13: the candidate is restarted
///   as a three-node cluster (`./start.sh --port/--data/--replicaof` on
///   7001-7003 + worker offset), then checked for replication (acked
///   writes visible on a replica within 3s), automatic failover (SIGKILL
///   the master; exactly one survivor reports ROLE master within 10s
///   holding every acked write; new writes reach the other node within
///   3s), rejoin (old master restarted pointed at the new leader returns
///   as a replica and converges), and regression (accumulated RESP suite
///   plus ./test.sh against the current master). Partition safety is out
///   of scope (deferred follow-up). Runs only when a ticket's acceptance
///   explicitly includes id 13; single-shot totals unchanged; acceptance
///   ids up to 13. Rows stamped `2026-10-14.11` or earlier are not
///   comparable without `backfill --yes`.
/// - `2026-10-14.11`: operator decisions (docs/OPEN-QUESTIONS.md).
///   (Q2) s3 durability pool 30 -> 36 turns (initial ~24 + 12 repair):
///   four consecutive cells showed refactors dying at the 30-turn wall.
///   (Q4) evaluation-protocol gating: `validate_comparison_wave` refuses
///   to pool runs across protocols (legacy rows group as `unknown`),
///   `merge_subruns` stamps mixed-protocol parents `mixed` instead of
///   silently taking the first, and portal rollups group + badge by
///   (rules regime, protocol). (Q5) throughput pricing is now linear:
///   score = base efficiency x speed_ratio (was r(1+2r): a 2x-faster
///   model could cost 10x per solve and tie). Rows stamped
///   `2026-10-14.10` or earlier are not comparable without
///   `backfill --yes`.
/// - `2026-10-14.10`: campaign ticket 9 (perf). New pipeline-level stage
///   12: pipelining in order (200 commands in one write, positional reply
///   checks via a frame-at-a-time reader — coalesced replies used to break
///   the shared one-shot reader), a sequential-GET p99 budget (<= 10ms over
///   300 GETs), and a post-bench throughput gate: the stage's own bench
///   measurement (same method as every stage) must reach >= PERF_GATE_RATIO
///   (1.3x) the genesis-stage measurement, carried in the campaign's
///   `baseline.json`. The gate only applies when a ticket's acceptance
///   includes id 12 and a baseline exists; single-shot totals unchanged;
///   acceptance ids up to 12. Rows stamped `2026-10-14.9` or earlier are
///   not comparable without `backfill --yes`.
/// - `2026-10-14.9`: campaign ticket 8 (security). New pipeline-level
///   stage 11, run with the candidate restarted under `SDB_PASSWORD` and a
///   64MB memory cap (restored afterwards so advisory probes see the
///   normal container): RESP auth gate, dashboard HTTP Basic auth,
///   oversized-frame probes (bulk header claiming 64MB / 2M-arg
///   multibulk must react without buffering), a malformed-frame corpus
///   with liveness after each payload, a 12-connection slow-loris probe,
///   and a container-log scan for the secret. Runs only when a ticket's
///   acceptance explicitly includes id 11; single-shot totals unchanged;
///   acceptance ids up to 11. Rows stamped `2026-10-14.8` or earlier are
///   not comparable without `backfill --yes`.
/// - `2026-10-14.8`: campaign ticket 7 (dashboard). New pipeline-level
///   stage 10: `GET /` (offline HTML, no absolute http(s) src/href), `GET
///   /api/stats` (JSON mirror of the INFO counters; deltas must track
///   scripted RESP traffic), `GET /events` (text/event-stream with a JSON
///   `data:` frame inside 2s while RESP traffic runs, all RESP commands
///   answered). The candidate container publishes the worker's 8090 port
///   only when a ticket's acceptance includes id 10. Runs only when a
///   ticket's acceptance explicitly includes id 10; single-shot totals
///   unchanged; acceptance ids up to 10. Rows stamped `2026-10-14.7` or
///   earlier are not comparable without `backfill --yes`.
/// - `2026-10-14.7`: stage-7 (INFO) verifier fix. The link-counter check
///   was unpassable: the test opened ONE connection before `info0` and
///   then required `total_connections_received` to grow by `info1`, with
///   nothing creating a second connection. It now opens and closes a
///   second client inside the window. Found by adding a counting-mock
///   test for the stage; stage 7 had never executed live on any run as of
///   this date, so no recorded cells are affected. Rows stamped
///   `2026-10-14.6` or earlier are not comparable without
///   `backfill --yes`.
/// - `2026-10-14.6`: campaign repair reserve floor 8 -> 12 (D6 amendment,
///   DECISIONS #12). Reserve is now `max(max_turns/4, 12)` capped at
///   `max_turns/2`, so initial caps for campaign tickets drop from 22->18
///   (30-turn pools), 17->13 (25), 30->28 (40). Evidence: three cells
///   where an 8-turn repair could not recover an interrupted refactor
///   (ling runs 4-5, inkling probe). Single-shot unchanged. Rows stamped
///   `2026-10-14.5` or earlier are not comparable without
///   `backfill --yes`.
/// - `2026-10-14.5`: stage-4 INCR state-independence fix. A repair
///   re-verification runs against a durable candidate whose WAL already
///   holds the seeded counter from the previous attempt (the seed is
///   stable per stage), so a bare `INCR` answered `:2` and the ticket was
///   scored as a false regression (live: s4 round 2, "Expected :1, got
///   :2"). The verifier now DELs the seeded counter first. Single-shot
///   (fresh candidate) semantics are identical; only re-verifications
///   change. Rows stamped `2026-10-14.4` or earlier are not comparable
///   without `backfill --yes`.
/// - `2026-10-14.4`: campaign tests & CI gate (ticket 6). New pipeline-
///   level stage 9: two offline `./test.sh` runs (deterministic, <=3min
///   each), coverage >=80% (Go measured via `go test -coverprofile` +
///   `go tool cover -func`; other languages read a self-reported
///   `coverage.txt` "coverage: NN.N%" line), and a revert-sensitivity
///   check (stage-1 expiry diff reversed in a scratch copy must make the
///   suite fail). Runs only when a ticket's acceptance explicitly
///   includes id 9; single-shot totals unchanged; acceptance ids up to 9.
///   Rows stamped `2026-10-14.3` or earlier are not comparable without
///   `backfill --yes`.
/// - `2026-10-14.3`: campaign ENOSPC incident (ticket 4). New redis
///   verifier stage 8 "ENOSPC incident (log volume full)": the pipeline
///   restarts the candidate with `SDB_DATA_DIR` on an 8MB tmpfs, fills it
///   with 240KB protocol writes, then requires (a) previously acked data
///   still reads back and PING stays alive, (b) the first unpersistable
///   write returns an error and is NOT applied (no silent ack), (c) later
///   writes answer without hanging. Runs only when a ticket's acceptance
///   explicitly includes id 8; single-shot totals unchanged; acceptance
///   ids up to 8. Rows stamped `2026-10-14.2` or earlier are not
///   comparable without `backfill --yes`.
/// - `2026-10-14.2`: campaign ticket 5 (observability) suite. New redis
///   verifier stage 7 "INFO metrics & counters": required-field presence,
///   integer parsing, monotonicity, and DELTAS against randomized scripted
///   traffic (hits/misses/commands/connections/expired) — static or
///   hardcoded counters fail. Stage 7 runs ONLY when a campaign ticket's
///   acceptance explicitly includes id 7, so single-shot redis totals stay
///   six stages and archived rows keep their verdicts. Campaign spec
///   validation now allows gaps in stage ids (slot 4 reserved for the
///   ENOSPC incident) and accepts acceptance ids up to 7. Rows stamped
///   `2026-10-14.1` or earlier are not comparable without `backfill --yes`.
/// - `2026-10-14.1`: measurement fix found by the 2026-10-14 canary review.
///   The n=100000 window kept the `get,set` mix, and the task MANDATES
///   fsync-before-ack: a correct candidate does ~870 SET/s, so the SET
///   phase took ~2 minutes per rep, tripped the 15s client timeout, and
///   every candidate measurement failed silently (all canary cells:
///   tp=None, five timeout-kill reps per log). Scored window is now
///   GET-only at n=100000 (read path = code-quality signal; stable);
///   the write path is a small-count SET probe reported but never scored
///   (fsync latency, not model skill). Client window timeout 15s -> 45s;
///   candidate measurement failure now logs loudly. Redis history rows
///   gain the load-shape stamp (`get-n100000`) so n=5000-era denominators
///   are never averaged with the new instrument. Rows stamped
///   `2026-10-14` or earlier are not comparable without `backfill --yes`.
/// - `2026-10-14`: review-round fixes (scoring + measurement). (1) The n=1
///   merge path in `merge_subruns` now honors the batch drift bracket
///   instead of dropping it — stop-on-pass cells (a 100% cell is usually a
///   single trial) published throughput scores divided by denominators the
///   batch itself had flagged (live: one opencode cell scored 12.58 on a
///   142k->201k drifted bracket while its 2-trial sibling was correctly
///   quarantined). (2) Persistent provider throttles under the opencode
///   harness are no longer model verdicts: a session with zero model tokens
///   and zero tool calls behind a 429/quota wall fails in the OMP billing
///   envelope (non-result), and the pipeline gates any zero-token +
///   empty-workspace session to `agent_failed` as defense in depth (live:
///   free-tier daily quota recorded 4 trials x ~50 min as ranked P@1 0%).
///   (3) Redis load windows n=5000 -> 100000 (measured 2.31x/CV 27.5% ->
///   1.26x/CV 5.8% spread, 10 windows each) because the r(1+2r) multiplier
///   amplifies denominator noise quadratically; `src/bench/mod.rs` and
///   `src/bench/batch_ref.rs` join GRADED_FILES so apparatus drift is
///   hashed like verifier drift. Rows stamped `2026-10-13` or earlier are
///   not comparable without `backfill --yes`; archived waves keep their
///   stamps (annotation only, never retroactive rescoring).
/// - `2026-10-13`: non-result exclusion + repair-attempt telemetry. Merges
///   drop harness/agent non-results (`agent_failed`, `agent_killed`,
///   `infra_error`, unknown futures) from every pass aggregate alongside
///   cheats — a fault's stages were never measured, so scoring it 0%
///   understated models (two archived 9-trial cells move 33.3%→50% and
///   66.7%→75% on re-merge). Parents rank on measured trials (fault counts
///   badged, trials retained in subruns); cells with no measurement stay
///   visible but unranked, and `solved_at` reports the trial number (not
///   the aggregate position). New `repair_attempted` flag on trial, parent,
///   and manifest rows: repair that ran but never converted is now visible
///   from the board (`repaired` still reports converted repairs only). Trial
///   pills for faults render neutral, never red. Rows stamped `2026-10-12.3`
///   or earlier are not comparable without `backfill --yes`; archived waves
///   keep their stamps (annotation only, never retroactive rescoring).
/// - `2026-10-12.3`: screening cap 4 + narrow auto-extend. Stop-on-pass
///   cells run at most 4 trials (was 3+escalate-to-5), stopping at the
///   first 100% pass — calibrated on wave data (every observed late solve
///   first-passed at trial 4). A 0/4 with every trial a harness fault
///   auto-extends within the SDB_AUTO_EXTEND_TRIALS envelope (once per
///   cell, audit-logged); mixed signals and model failures stay manual.
///   Trial counts change denominators, so rows stamped `2026-10-12.2` or
///   earlier are not comparable. Rank key unchanged (P@1-lo); capability
///   (solved@N) is a display co-column only.
/// - `2026-10-12.2`: READY early-exit + pooled turn budgets. Every task
///   prompt (and the feedback-repair prompt) gains a READY-signal section:
///   models touch `/workspace/READY` when done and verification starts
///   immediately instead of burning the full turn budget. Initial attempts
///   are capped at min(max_turns, 50) with feedback_rounds > 0; repair
///   rounds draw from the shared pool remainder (cap 25/round), and failed
///   repair rounds now count their partial turns/tokens against the pool.
///   Opencode READY exits break Ok (omp parity): an empty workspace after
///   a model-signaled finish records failed_tests, not agent_failed.
///   Also records the robust port-readiness probing shipped in 3e6ca67
///   (candidates are probed until actually listening instead of scored
///   0% on premature probes: docker-proxy accepts the handshake before
///   the in-container service is up, so compile-on-start candidates were
///   false negatives — check suspect rows via replay). Early exits shift
///   pass rates vs full-budget runs; rows stamped `2026-10-12.1` or
///   earlier are not comparable.
/// - `2026-10-12.1`: sequential capability protocol. Headline protocol evaluates
///   models up to 3 tries, stopping at first 100% pass (capping compute),
///   and escalates to 5 tries only if all 3 fail (discriminating hard tasks).
///   Captures `solved`, `solved_at` (1..5), `best_stage_score`, and `protocol`.
///   Reliability and harness metrics are reported alongside as harness diagnostics.
/// - `2026-10-11.2`: proxy fixture determinism. Stages 1-2 learn markers
///   from identical in-process stubs when host publishes decay (docker
///   pool unchanged for the candidate path); dead containers fail loud
///   naming the fixture. Stage 4 kills via pause/unpause (no re-publish
///   races) with rejoin proven end-to-end via proxy (stricter than the
///   direct dial: slow rejoin now fails). Candidate-observable wire
///   behavior otherwise identical; verdicts change only where fixtures,
///   not models, were failing.
/// - `2026-10-11.1`: AGENTS.md worker ports. Both runners wrote the brief
///   attachment with canonical ports while the prompt carried worker ports
///   (measured live: a proxy trial read 8191 in the brief vs 8091 in
///   AGENTS.md and hedged on both). AGENTS.md now renders through the
///   same alias-shielded templating; `template_ports_for_worker` moved to
///   config.rs (shared, no new import edges) with per-task render audits.
///   Rows stamped `2026-10-09.4` or earlier keep their stamps; reruns pick
///   up the fix.
/// - `2026-10-09.4`: trial-level cheat exclusion + proxy verifiability.
///   Merges drop `disqualified_cheat` subruns from pass aggregates and
///   rank parents on valid trials (visible DQ counts + tooltips; all-cheat
///   parents stay excluded); cheat top-ups (trial10+) merge under the same
///   rule. Proxy prompt port-templating narrowed to alias tokens (the
///   whole-line "upstream" skip told models 8091 AND the worker port),
///   plus a verify-time upstream health gate (host publishes decay under
///   daemon churn) and stage-log prefix dedup. Rows stamped `2026-10-09.3`
///   or earlier recompute under backfill; trial rows keep their stamps.
/// - `2026-10-09.3`: machine effort parity + cutoff recovery. OMP passes
///   `--thinking=<requested>` unclamped (the CLI downgrades unsupported
///   levels itself — measured, no HTTP 400 on this path); OpenCode maps
///   benchmark effort to model `#variant` (native `reasoning.effort`,
///   per-trial config, prose fallback + one prose-only retry on
///   resolution failure). Turn-1 reasoning cutoffs (exit 0, 0 tools, empty
///   workspace) resume the same session with a tool-first nudge, cap 2 —
///   recovery over reclassification, so no archived row is rescored.
///   Turn-telemetry fix and cutoff annotation ride along with no grading
///   impact. NOTE: the fingerprint hashes prompt templates, verifiers and
///   scoring — not the composed per-trial prompt (prose + variant + flag)
///   or the resume policy — so this stamp bump carries those; rows stamped
///   `2026-10-09.2` or earlier are not comparable without `backfill --yes`.
/// - `2026-10-09.2`: canary markers in every graded prompt (1c). Each
///   `tasks/*/prompt.md` carries a frozen per-task GUID; the archived
///   workspace is scanned for echoes and the finding recorded on the row
///   (`canary_echoed`, advisory — never a verdict). Prompt content is a
///   graded input, so the prompt hashes move with this stamp; rows stamped
///   `2026-10-09.1` or earlier are not comparable without `backfill --yes`.
/// - `2026-10-09.1`: verdict honesty (audit round). Compliance scoping:
///   test-stub imports (`stub.py` http.server) pass, import-statement lines
///   never count as daemon execution (`internal/httpd`). Trial rows carry
///   `status`; merges propagate `disqualified_cheat`; ranked boards
///   (leaderboard, portal, matrix) exclude cheats instead of scoring them
///   as active; model packaging faults score `failed_tests`, not
///   `infra_error`; 100%-verified candidates score `completed` however the
///   agent died. Companion `repair` command cures false positives and
///   consolidates loose trials. Results stamped `2026-10-03.9` or earlier
///   are not comparable without `backfill --yes`.
/// - `2026-10-03.9`: phantom-row exclusion (legacy single-shot JSONs with no
///   subruns/no trial count no longer publish; their bare 100% pass_rate
///   drew green 100%-n=0 cells that were never trials). No leniency change
///   to honest rows; only unscored debris filtered.
/// - `2026-10-03.8`: cheat-surface closure (post-.7 verifier edits that
///   shipped unbumped in 6dbbc15 — process miss, corrected here). Compliance
///   path whitelist removed (any daemon basename fails, whatever the path;
///   proven `/tmp/redis-server` bypass dead). WAL recovery exact-count to
///   at-least (extra un-acked messages no longer fail a durable broker).
///   Single-trial score unified to P@1/eff (n=1 partial-credit rows no longer
///   outrank merges at the same P@1). Enqueue dedupe, CLI-extend drift
///   stickiness, eval-seed randomization. No leniency change to honest rows.
/// - `2026-10-03.7`: review-trust pins (stricter gates, no leniency change
///   to honest rows). Shared `canonical_best()` (merge + review-run converge;
///   fixes pass_rate-only divergence on tied 100%s). HTTP keep-alive now a
///   real same-socket test (raw socket; the reqwest loop re-tested 200x3).
///   Pricing badges (`[free]`, `[contrib]`, display-only). Review workspace
///   reads trial `workspace/` (was truncated console.log) and skips *.log.
/// - `2026-10-03.6`: dns task removed (code + prompt + verifier deleted, data
///   archived). `tasks/dns/prompt.md` + `src/verifier/dns.rs` leave
///   GRADED_FILES with the two entries below; archived dns rows (plain
///   `"task": "dns"` strings in results/DB) keep loading untouched — no
///   `TaskType` variant is involved in row parsing.
/// - `2026-10-03.5`: verifier pins (stricter gates, no leniency change to
///   honest rows). Proxy + HTTP: keep-alive (3 sequential 200s, one
///   connection). PDF: dynamic-content (distinct nonces, distinct bodies).
///   Redis INCR/AOF reviewed already-strict, untouched. Curve unchanged:
///   zero anchored ratios in the fresh wave (5/5 drifted), so any retune
///   is unverifiable. Drift is reference-instability under parallel load
///   (windows already serialized under the bench lock), not tolerance.
/// - `2026-10-03.4`: retired-task freeze (dns). `tasks/dns/prompt.md` +
///   `src/verifier/dns.rs` frozen in GRADED_FILES (archived rows + panic
///   guard); no-edit policy so a dead task can never roll live stamps again.
/// - `2026-10-03.3`: merged-row consistency (advisor round). Step 5
///   (duration/turns/tokens/savings) joins step 4 on the all-trial
///   denominator — a failed trial's seconds/tokens feed effective cost
///   and now also the duration column (no more "cheap-seconds"); plus
///   the `$/solved` column fix (restored `Score / \u00a2` header dropped
///   by a bad dedup: 17 cells vs 16 headers).
/// - `2026-10-03.2`: verifier hardening. DNS stage 1 requires an answer
///   section (packet echo scores 0); pubsub stage 1 handshakes a fresh
///   random key (hard-coded `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=` stub fails);
///   proxy stage 1 pinned by a static-body negative test; the compliance
///   scanner covers `vendor/` + hidden dirs and rejects escaping symlinks.
///   Stricter gates: previously passing stubs now fail.
/// - `2026-10-03.1`: fair cost is the mean over ALL trials, not just
///   passing ones. The old passing-only mean understated real spend
///   (median 1.44x, up to 6.7x on mixed outcomes): failed trials still
///   burn API + compute, and failure is already priced by the Pass@1
///   multiplier — hiding failed spend double-counted success. Every mixed
///   parent row gets *more expensive* and its headline score drops; the
///   score is now P@1 per mean trial cost = 1 / (cost per solved
///   task). Results stamped `2026-09-30.1` or earlier are not comparable.
/// - `2026-09-30.1`: effective cost charges cache-read tokens at
///   `COMPUTE_COST_CACHED_FRACTION` of the baseline instead of the full
///   `COMPUTE_COST_PER_TOKEN`, so cached context is no longer billed twice
///   (the API path already discounted it 75-90% and the Cache Savings column
///   reported that discount). Every efficiency score shifts: runs with heavy
///   context caching get *cheaper*, so scores rise. Results stamped
///   `2026-09-28.1` or earlier are not comparable with these.
///   GRADED_FILES also gained `src/cost/calculator.rs` and
///   `src/report/leaderboard.rs`, so the stamp now covers the scoring path
///   and not just the verifiers.
/// - `2026-09-28.1`: binproto throughput probe; repro fields
///   (temperature/top_p/seed/prompt_sha) on manifests; runs-table
///   probe columns. New publish baseline.
/// - `2026-09-22.1`: first fingerprinted version (post cheat-stage).
/// - `2026-09-24.2`: binproto task (SDB1 binary RPC) added.
/// - `2026-09-24.1`: proportional efficiency credit (98% earns 98%, no
///   100%-or-zero cliff); P@1-lo leads the pass column; throughput tagged by
///   language; DNS unquarantined post-fix.
/// - `2026-09-22.2`: DNS candidate joins run network (ref-dns reachable);
///   fail-fast forwarding guidance in the DNS prompt; stage-5 latency logging.
///
pub const RULES_VERSION: &str = "2026-10-19.44";

/// R27 F3: the rules-page statement of the cap's price basis. Kept as a
/// constant so the page, the proposal and the test cannot drift.
pub const CAP_PRICE_BASIS: &str = "The $0.99 cell cap is at list price from src/cost/model_prices.json; the provider bill is recorded for audit only.";

/// R28 F1: the linked SQLite oracle that grades every answer. The crate
/// versions are covered by `Cargo.lock` in the compiled digest.
pub fn sqlite_version() -> &'static str {
    rusqlite::version()
}

/// R28-1: a run shows its recorded oracle, never the current binary's.
pub fn recorded_sqlite_version(recorded: Option<&str>) -> &str {
    recorded.unwrap_or("\u{2014}")
}

/// R28-3/R29 F1: compare the date and the numeric suffix separately, so
/// `.100` is later than `.39` (a string comparison is not enough).
pub fn rules_version_at_least(version: &str, minimum: &str) -> bool {
    fn split(value: &str) -> Option<(&str, u64)> {
        let (date, number) = value.rsplit_once('.')?;
        Some((date, number.parse().ok()?))
    }
    match (split(version), split(minimum)) {
        (Some((vd, vn)), Some((md, mn))) => vd > md || (vd == md && vn >= mn),
        _ => false,
    }
}

/// R28-1: a run's recorded cap basis. Old rows that predate the list-price
/// policy (`.39`) get none; `.39` and later can derive it from the rules
/// version when the field was not recorded yet.
pub fn recorded_cap_basis<'a>(rules: &str, recorded: Option<&'a str>) -> Option<&'a str> {
    recorded.or_else(|| rules_version_at_least(rules, "2026-10-19.39").then_some(CAP_PRICE_BASIS))
}

/// One shared list, also read by `build.rs`: `file`/`dir` entries are
/// compiled in and hashed at build time; `runtime-file`/`runtime-dir`
/// entries are hashed from SDB_HOME when the stamp is computed.
const GRADED_MANIFEST: &str = include_str!("graded-files.txt");

/// The SHA-256 of the compiled-in graded inputs, emitted by `build.rs`.
pub const COMPILED_GRADED_DIGEST: &str = env!("SDB_COMPILED_RULES_DIGEST");

/// Keep 128 bits of the SHA-256 digest in the public stamp (the round asks
/// for at least 64).
const STAMP_BYTES: usize = 16;

/// Hash of the graded inputs, as `"{RULES_VERSION}+{hex32}"`.
pub fn fingerprint() -> String {
    fingerprint_root(&crate::config::get_repo_root())
}

/// The static files plus every regular file under the manifest's
/// directories, as paths relative to `root`, sorted and deduplicated.
pub fn graded_files(root: &Path) -> Vec<PathBuf> {
    graded_hash::files(root, GRADED_MANIFEST, true)
}

fn runtime_files(root: &Path) -> Vec<PathBuf> {
    graded_hash::runtime_files(root, GRADED_MANIFEST)
}

fn hash_entry(hasher: &mut Sha256, rel: &Path, content: &[u8]) {
    hasher.update(rel.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(content);
    hasher.update([0]);
}

fn stamp(digest: &[u8]) -> String {
    let hex: String = digest
        .iter()
        .take(STAMP_BYTES)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{RULES_VERSION}+{hex}")
}

/// Hash exactly these files under `root` (relative paths). The tests use
/// this form to perturb a temporary copy without touching the real tree.
pub fn fingerprint_files(root: &Path, files: &[PathBuf]) -> String {
    let mut files = files.to_vec();
    files.sort();
    files.dedup();
    let mut hasher = Sha256::new();
    hasher.update(RULES_VERSION.as_bytes());
    hasher.update([0]);
    for f in files {
        // A missing graded file must not hash as empty content: that would
        // leave the stamp unchanged when a verifier is deleted or renamed.
        let content = fs::read(root.join(&f)).unwrap_or_else(|e| {
            panic!("graded file {} is unreadable: {e}", f.display());
        });
        hash_entry(&mut hasher, &f, &content);
    }
    stamp(&hasher.finalize())
}

/// The stamp over a compiled digest, an SQLite oracle version and the
/// runtime files. Split from [`fingerprint_root`] so the oracle-version
/// test can vary the two inputs without rebuilding the binary.
fn stamp_with_oracle(root: &Path, compiled: &str, oracle: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(RULES_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(compiled.as_bytes());
    hasher.update([0]);
    hasher.update(oracle.as_bytes());
    hasher.update([0]);
    for f in runtime_files(root) {
        let Ok(content) = fs::read(root.join(&f)) else {
            continue;
        };
        hash_entry(&mut hasher, &f, &content);
    }
    stamp(&hasher.finalize())
}

/// Hash the binary's compiled-in graded digest, the linked SQLite oracle
/// and the runtime files under `root` (SDB_HOME). The runtime path
/// tolerates a minimal SDB_HOME: a missing directory contributes nothing.
pub fn fingerprint_root(root: &Path) -> String {
    stamp_with_oracle(root, COMPILED_GRADED_DIGEST, sqlite_version())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rules_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(root: &Path, rel: &str, content: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        PathBuf::from(rel)
    }

    #[test]
    fn test_fingerprint_starts_with_version() {
        let fp = fingerprint();
        assert!(
            fp.starts_with(&format!("{RULES_VERSION}+")),
            "fingerprint {fp} must start with RULES_VERSION"
        );
    }

    /// R27 F1 acceptance: SHA-256 is deterministic across processes. The
    /// expected value is pinned for a one-file fixture tree.
    #[test]
    fn r27_f1_stamp_is_stable_across_processes() {
        let root = temp_root("sha");
        let rel = put(&root, "src/scoring.rs", "fixture\n");
        let fp = fingerprint_files(&root, std::slice::from_ref(&rel));
        assert_eq!(
            fp, "2026-10-19.44+769e9623e9f76d5c8620fdf5a53abbd6",
            "the SHA-256 stamp must be stable"
        );
        assert_eq!(fp, fingerprint_files(&root, &[rel]));
        let _ = fs::remove_dir_all(&root);
    }

    /// R27 F1 acceptance: a compiled-in file changed on disk after the
    /// build does not move the binary's stamp; a rebuild (simulated by
    /// fingerprint_files over the compiled list) does.
    #[test]
    fn r27_f1_compiled_in_files_hash_the_build() {
        assert_eq!(COMPILED_GRADED_DIGEST.len(), 64);
        assert!(COMPILED_GRADED_DIGEST
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        let root = temp_root("compiled");
        let rel = put(&root, "src/scoring.rs", "one\n");
        let compiled_a = fingerprint_files(&root, std::slice::from_ref(&rel));
        let runtime_a = fingerprint_root(&root);
        put(&root, "src/scoring.rs", "two\n");
        let compiled_b = fingerprint_files(&root, &[rel]);
        let runtime_b = fingerprint_root(&root);
        assert_eq!(
            runtime_a, runtime_b,
            "a compiled file on disk must not change the built binary's stamp"
        );
        assert_ne!(
            compiled_a, compiled_b,
            "a rebuild over the changed file must move the stamp"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// R27 F3 acceptance: the rules page and proposal state the cap's
    /// list-price basis and its snapshot source.
    #[test]
    fn r27_f3_rules_page_states_list_price() {
        assert!(CAP_PRICE_BASIS.contains("list price"));
        assert!(CAP_PRICE_BASIS.contains("src/cost/model_prices.json"));
        let proposal = include_str!("../../docs/PROPOSAL-sdb-sql.md");
        assert!(
            proposal.contains("list price"),
            "proposal missing list price"
        );
        assert!(
            proposal.contains("src/cost/model_prices.json"),
            "proposal missing the snapshot source"
        );
    }

    fn normalize_include(base: &Path, target: &str) -> PathBuf {
        let joined = base.join(target);
        let mut out = PathBuf::new();
        for part in joined.components() {
            match part {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    }

    fn include_targets(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for prefix in ["include_str!(\"", "include_bytes!(\""] {
            let mut rest = text;
            while let Some(at) = rest.find(prefix) {
                let start = at + prefix.len();
                let Some(end) = rest[start..].find('"') else {
                    break;
                };
                out.push(rest[start..start + end].to_string());
                rest = &rest[start + end..];
            }
        }
        out
    }

    /// R27 F1/R27-2 acceptance: every source under the verifier/prompts/
    /// cost directories and every `include_str!`/`include_bytes!` target
    /// under pipeline/sandbox/cost/prompts is covered by the one list.
    #[test]
    fn r27_f1_every_graded_source_is_listed() {
        let root = crate::config::get_repo_root();
        let files = graded_files(&root);
        for dir in ["src/verifier/sql", "src/prompts", "src/cost"] {
            let mut inner = Vec::new();
            graded_hash::collect_dir(&root, Path::new(dir), &mut inner);
            assert!(!inner.is_empty(), "no files under {dir}");
            for rel in inner {
                assert!(
                    files.contains(&rel),
                    "{rel:?} under {dir} is missing from graded-files.txt"
                );
            }
        }
        let mut sources = vec![PathBuf::from("src/pipeline.rs")];
        for dir in ["src/sandbox", "src/cost", "src/prompts"] {
            graded_hash::collect_dir(&root, Path::new(dir), &mut sources);
        }
        for source in sources {
            let Ok(text) = fs::read_to_string(root.join(&source)) else {
                continue;
            };
            for target in include_targets(&text) {
                let rel = normalize_include(source.parent().unwrap_or(Path::new("")), &target);
                assert!(
                    files.contains(&rel),
                    "{source:?} includes {target:?} -> {rel:?}, which is missing from graded-files.txt"
                );
            }
        }
    }

    /// R28-1 acceptance: recorded run values win; a pre-`.39` row gets no
    /// list-price label even though today's binary uses the snapshot.
    #[test]
    fn r28_r1_recorded_oracle_and_cap_basis() {
        assert_eq!(recorded_sqlite_version(Some("3.0.0")), "3.0.0");
        assert_eq!(recorded_sqlite_version(None), "—");
        assert_eq!(
            recorded_cap_basis("2026-10-19.40", None),
            Some(CAP_PRICE_BASIS)
        );
        assert_eq!(recorded_cap_basis("2026-10-19.38", None), None);
        assert_eq!(
            recorded_cap_basis("2026-10-19.38", Some("recorded")),
            Some("recorded")
        );
    }

    /// R29 F1 acceptance: rules versions compare as (date, number).
    #[test]
    fn r29_f1_rules_versions_compare_numerically() {
        assert!(rules_version_at_least("2026-10-19.100", "2026-10-19.39"));
        assert!(!rules_version_at_least("2026-10-18.50", "2026-10-19.39"));
        assert!(rules_version_at_least("2026-10-19.39", "2026-10-19.39"));
        assert!(!rules_version_at_least("legacy-pre-cheat", "2026-10-19.39"));
        assert_eq!(
            recorded_cap_basis("2026-10-19.38", None),
            None,
            "the old string comparison must not make pre-.39 look priced"
        );
    }

    /// R29 F1 acceptance: a missing compiled `dir` entry fails the build
    /// digest instead of hashing as nothing.
    #[test]
    fn r29_f1_missing_graded_dir_fails() {
        let root = temp_root("missingdir");
        let result = std::panic::catch_unwind(|| {
            graded_hash::compiled_digest(&root, "dir definitely_missing\n")
        });
        assert!(result.is_err(), "a missing dir must fail compiled_digest");
        let _ = fs::remove_dir_all(&root);
    }

    /// R30-1 acceptance: the pinned toolchain is in the graded manifest.
    #[test]
    fn r30_r1_toolchain_is_a_graded_input() {
        let root = crate::config::get_repo_root();
        let files = graded_files(&root);
        assert!(
            files.iter().any(|p| p == Path::new("rust-toolchain.toml")),
            "graded_files misses rust-toolchain.toml"
        );
        assert!(
            GRADED_MANIFEST
                .lines()
                .any(|line| line.trim() == "file rust-toolchain.toml"),
            "graded-files.txt misses the toolchain entry"
        );
    }

    /// R28 F1 acceptance: both the linked SQLite version and the compiled
    /// crate digest move the stamp.
    #[test]
    fn r28_f1_oracle_version_moves_the_stamp() {
        let root = temp_root("oracle");
        let a = stamp_with_oracle(&root, "compiled-a", "3.45.0");
        let b = stamp_with_oracle(&root, "compiled-a", "3.45.1");
        let c = stamp_with_oracle(&root, "compiled-b", "3.45.0");
        assert_ne!(a, b, "the SQLite version must move the stamp");
        assert_ne!(a, c, "the crate digest must move the stamp");
        assert!(!sqlite_version().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    /// R28 F4 acceptance: the test recomputes the build digest from the
    /// same source tree, manifest and hashing module and compares it with
    /// the compile-time constant.
    #[test]
    fn r28_f4_build_digest_matches_the_tree() {
        let root = crate::config::get_repo_root();
        assert_eq!(
            graded_hash::compiled_digest(&root, GRADED_MANIFEST),
            COMPILED_GRADED_DIGEST,
            "build.rs and the test must hash the same compiled inputs"
        );
        let different_manifest = "file src/scoring.rs\n";
        assert_ne!(
            graded_hash::compiled_digest(&root, different_manifest),
            COMPILED_GRADED_DIGEST,
            "a manifest that disagrees with the build must produce a different digest"
        );
    }

    /// R27-1 acceptance: the campaign spec is read from SDB_HOME at run
    /// time, so editing it must move the runtime half of the stamp.
    #[test]
    fn r27_r1_campaign_json_under_sdb_home_changes_the_stamp() {
        let root = temp_root("campaignhome");
        let rel = put(
            &root,
            "campaigns/sdb-sql/campaign.json",
            r#"{"name":"sdb-sql","stages":[]}"#,
        );
        let a = fingerprint_root(&root);
        put(
            &root,
            rel.to_str().unwrap(),
            r#"{"name":"sdb-sql","stages":[{}]}"#,
        );
        let b = fingerprint_root(&root);
        assert_ne!(a, b, "a runtime campaign.json edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    /// R27 F1 acceptance: a stage prompt under SDB_HOME still moves the
    /// runtime half of the stamp.
    #[test]
    fn r27_f1_a_stage_under_sdb_home_changes_the_stamp() {
        let root = temp_root("stagehome");
        let rel = put(&root, "campaigns/sdb-sql/stages/0_genesis/prompt.md", "one");
        let a = fingerprint_root(&root);
        put(&root, rel.to_str().unwrap(), "two");
        let b = fingerprint_root(&root);
        assert_ne!(a, b, "a runtime stage edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn r24_r1_the_real_tree_covers_the_sdb_sql_inputs() {
        let files = graded_files(&crate::config::get_repo_root());
        for rel in [
            "campaigns/sdb-sql/campaign.json",
            "campaigns/sdb-sql/stages/0_genesis/prompt.md",
            "src/verifier/sql/wire.rs",
            "src/scoring.rs",
            "src/cost/model_prices.json",
            "src/prompts/session_budget.txt",
            "src/prompts/omp_effective.md",
            "src/prompts/opencode_agents.md",
            "tasks/sql/prompt.md",
        ] {
            assert!(
                files.iter().any(|p| p == Path::new(rel)),
                "graded_files misses {rel}: {files:?}"
            );
        }
    }

    #[test]
    fn r24_r2_editing_the_price_snapshot_changes_the_fingerprint() {
        let root = temp_root("prices");
        let rel = put(&root, "src/cost/model_prices.json", r#"{"models":[]}"#);
        let a = fingerprint_files(&root, std::slice::from_ref(&rel));
        put(&root, "src/cost/model_prices.json", r#"{"models":[1]}"#);
        let b = fingerprint_files(&root, &[rel]);
        assert_ne!(a, b, "a price-snapshot edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn r24_r1_editing_a_stage_prompt_changes_the_fingerprint() {
        let root = temp_root("stage");
        let rel = put(
            &root,
            "campaigns/sdb-sql/stages/99_new/prompt.md",
            "first prompt",
        );
        let a = fingerprint_files(&root, std::slice::from_ref(&rel));
        put(
            &root,
            "campaigns/sdb-sql/stages/99_new/prompt.md",
            "second prompt",
        );
        let b = fingerprint_files(&root, &[rel]);
        assert_ne!(a, b, "a stage prompt edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn r24_r1_editing_an_sql_verifier_changes_the_fingerprint() {
        let root = temp_root("sql");
        let rel = put(&root, "src/verifier/sql/wire.rs", "one verifier");
        let a = fingerprint_files(&root, std::slice::from_ref(&rel));
        put(&root, "src/verifier/sql/wire.rs", "two verifier");
        let b = fingerprint_files(&root, &[rel]);
        assert_ne!(a, b, "an SQL verifier edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn r24_r1_editing_a_scoring_constant_changes_the_fingerprint() {
        let root = temp_root("scoring");
        let rel = put(&root, "src/scoring.rs", "pub const TURN_GUARD: u32 = 200;");
        let a = fingerprint_files(&root, std::slice::from_ref(&rel));
        put(&root, "src/scoring.rs", "pub const TURN_GUARD: u32 = 201;");
        let b = fingerprint_files(&root, &[rel]);
        assert_ne!(a, b, "a scoring constant edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    /// R26 R23-7 acceptance: the remaining turn rule is a graded input, so
    /// editing how the guard is shared moves the stamp.
    #[test]
    fn r26_editing_the_turn_rule_changes_the_fingerprint() {
        let root = temp_root("turnrule");
        let rel = put(
            &root,
            "src/scoring.rs",
            "pub fn initial_turn_limit(max_turns: u32) -> Option<u32> { (max_turns > 0).then_some(max_turns) }",
        );
        let a = fingerprint_files(&root, std::slice::from_ref(&rel));
        put(
            &root,
            "src/scoring.rs",
            "pub fn initial_turn_limit(max_turns: u32) -> Option<u32> { (max_turns > 1).then_some(max_turns) }",
        );
        let b = fingerprint_files(&root, &[rel]);
        assert_ne!(a, b, "a turn-rule edit must move the stamp");
        let _ = fs::remove_dir_all(&root);
    }

    /// R43 F4: the gate's and flake bar's scope list
    /// (`scripts/graded-paths.txt`) covers every input the fingerprint
    /// hashes (`graded-files.txt`), so an edit can never move the stamp
    /// while the gate calls it non-graded.
    #[test]
    fn r43_f4_graded_lists_agree() {
        let root = crate::config::get_repo_root();
        let paths = fs::read_to_string(root.join("scripts/graded-paths.txt"))
            .expect("scripts/graded-paths.txt is readable");
        let bases: Vec<&str> = paths
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(!bases.is_empty(), "graded-paths.txt has no entries");
        let mut checked = 0;
        for line in GRADED_MANIFEST.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let rel = ["runtime-file", "runtime-dir", "file", "dir"]
                .iter()
                .find_map(|kind| line.strip_prefix(&format!("{kind} ")))
                .unwrap_or_else(|| panic!("unrecognised graded entry {line:?}"));
            checked += 1;
            assert!(
                bases
                    .iter()
                    .any(|b| rel == *b || rel.starts_with(&format!("{b}/"))),
                "graded-files entry {rel:?} is not under any graded path: {bases:?}"
            );
        }
        assert!(checked >= 10, "only {checked} graded entries parsed");
    }

    #[test]
    fn r24_r1_a_new_stage_directory_is_covered() {
        let root = temp_root("newstage");
        put(
            &root,
            "campaigns/sdb-sql/stages/99_new/prompt.md",
            "new stage",
        );
        let files = graded_files(&root);
        assert!(
            files
                .iter()
                .any(|p| p == Path::new("campaigns/sdb-sql/stages/99_new/prompt.md")),
            "a new stage directory must be swept: {files:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
