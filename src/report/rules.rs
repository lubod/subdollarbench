//! Rule-set fingerprint: which prompts + verifier sources graded a run.
//!
//! `RULES_VERSION` is bumped explicitly whenever grading changes
//! (prompt edit, verifier logic, cheat-stage enable). `fingerprint()`
//! hashes the graded inputs so CI fails if content drifts without a bump.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Bump this whenever grading changes. History:
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
///   score is now pass% x P@1 per mean trial cost = 1 / (cost per solved
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
pub const RULES_VERSION: &str = "2026-10-03.3";

/// Files whose content defines the published numbers: the prompts the model
/// saw, the verifiers that judged it, *and* the code that turns a result into
/// a score. `leaderboard.rs` treats `rules_version` as the comparability key,
/// so anything that can change a score has to be in here - otherwise two runs
/// scored by different maths carry the same stamp.
const GRADED_FILES: &[&str] = &[
    "tasks/binproto/prompt.md",
    "tasks/dns/prompt.md",
    "tasks/http/prompt.md",
    "tasks/pdf/prompt.md",
    "tasks/proxy/prompt.md",
    "tasks/pubsub/prompt.md",
    "tasks/redis/prompt.md",
    "tasks/wal/prompt.md",
    "src/verifier/compliance.rs",
    "src/verifier/binproto.rs",
    "src/verifier/dns.rs",
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
