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
pub mod crash;
pub mod driver;
pub mod generator;
pub mod oracle;
pub mod ref_server;
pub mod run;
pub mod scenarios;
pub mod schema;
pub mod sentinel;
pub mod shrink;
#[cfg(test)]
mod toy;
pub mod txn;
pub mod wire;
