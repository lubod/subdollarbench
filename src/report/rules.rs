//! Rule-set fingerprint: which prompts + verifier sources graded a run.
//!
//! `RULES_VERSION` is bumped explicitly whenever grading changes
//! (prompt edit, verifier logic, cheat-stage enable). `fingerprint()`
//! hashes the graded inputs so CI fails if content drifts without a bump.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Bump this whenever grading changes. History:
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
pub const RULES_VERSION: &str = "2026-10-09.3";

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
