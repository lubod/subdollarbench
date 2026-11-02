//! sdb-sql verifier core (docs/PROPOSAL-sdb-sql.md): a seeded schema, a
//! type-directed statement generator, and an in-process SQLite oracle the
//! harness compares the candidate against (differential verification).
//!
//! Module layout (built in order): `schema` (seeded tables/columns),
//! `oracle` (rusqlite reference + DDL translation + error-class mapping +
//! discard rules), `generator`, `wire`, `compare`, `run` (replay against a
//! `Candidate`), `client` (a TCP `Candidate` for the model's server),
//! `shrink` (minimal repros), and later `scenarios`. The
//! self-tests gate the verifier itself: oracle-vs-oracle agreement, an
//! independent toy engine (`toy`, test-only) that must agree with the
//! oracle, 26 planted-bug mutants that must be caught, profile-coverage
//! assertions and per-stage comparable-check floors.

pub mod client;
pub mod compare;
pub mod concurrency;
pub mod crash;
pub mod disk;
pub mod driver;
pub mod generator;
pub mod memory;
pub mod observability;
pub mod oracle;
pub mod perf;
pub mod perfscore;
pub mod pgwire;
pub mod ref_server;
pub mod replication;
pub mod run;
pub mod scenarios;
pub mod schema;
pub mod security;
pub mod sentinel;
pub mod shrink;
#[cfg(test)]
mod toy;
pub mod txn;
pub mod wire;

/// R61 F8: phase timing for the live proofs (measure only). With
/// `SDB_SQL_TIMING=1` the phase prints to stderr when it drops, so a
/// proof's `--nocapture` output shows where its time went. Unset, it
/// changes nothing.
pub struct PhaseTimer(String, std::time::Instant);

impl PhaseTimer {
    pub fn new(label: impl Into<String>) -> Self {
        Self(label.into(), std::time::Instant::now())
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        if std::env::var_os("SDB_SQL_TIMING").is_some() {
            eprintln!(
                "[TIMING] {}: {:.3}s",
                self.0,
                self.1.elapsed().as_secs_f64()
            );
        }
    }
}
