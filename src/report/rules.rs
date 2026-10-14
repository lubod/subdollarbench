//! Rule-set fingerprint: which prompts + verifier sources graded a run.
//!
//! `RULES_VERSION` is bumped explicitly whenever grading changes
//! (prompt edit, verifier logic, cheat-stage enable). `fingerprint()`
//! hashes the graded inputs so CI fails if content drifts without a bump.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Bump this whenever grading changes. History:
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
pub const RULES_VERSION: &str = "2026-10-14.2";

/// Files whose content defines the published numbers: the prompts the model
/// saw, the verifiers that judged it, *and* the code that turns a result into
/// a score. `leaderboard.rs` treats `rules_version` as the comparability key,
/// so anything that can change a score has to be in here - otherwise two runs
/// scored by different maths carry the same stamp.
const GRADED_FILES: &[&str] = &[
    "tasks/binproto/prompt.md",
    "tasks/http/prompt.md",
    "tasks/pdf/prompt.md",
    "tasks/proxy/prompt.md",
    "tasks/pubsub/prompt.md",
    "tasks/redis/prompt.md",
    "tasks/wal/prompt.md",
    "src/verifier/compliance.rs",
    "src/verifier/binproto.rs",
    "src/verifier/http.rs",
    "src/verifier/pdf.rs",
    "src/verifier/proxy.rs",
    "src/verifier/pubsub.rs",
    "src/verifier/redis.rs",
    "src/verifier/wal.rs",
    // The scoring and ranking path, not just the judging.
    "src/cost/calculator.rs",
    "src/report/leaderboard.rs",
    // The measurement apparatus: load-test shape and batch-bracket math
    // move every recorded denominator, so they are graded inputs too.
    "src/bench/mod.rs",
    "src/bench/batch_ref.rs",
];

/// Hash of the graded inputs, as `"{RULES_VERSION}+{hex8}"`.
pub fn fingerprint() -> String {
    let root = crate::config::get_repo_root();
    let mut h = DefaultHasher::new();
    RULES_VERSION.hash(&mut h);
    for f in GRADED_FILES {
        // A missing graded file must not hash as empty content: that would
        // leave the stamp unchanged when a verifier is deleted or renamed, and
        // results graded by a different code would compare as equal.
        let content = std::fs::read_to_string(root.join(f)).unwrap_or_else(|e| {
            panic!("graded file {f} is unreadable: {e}");
        });
        f.hash(&mut h);
        content.hash(&mut h);
    }
    format!("{RULES_VERSION}+{:08x}", h.finish() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_starts_with_version() {
        let fp = fingerprint();
        assert!(
            fp.starts_with(&format!("{RULES_VERSION}+")),
            "fingerprint {fp} must start with RULES_VERSION"
        );
    }
}
