//! sdb-sql verifier core (docs/PROPOSAL-sdb-sql.md): a seeded schema, a
//! type-directed statement generator, and an in-process SQLite oracle the
//! harness compares the candidate against (differential verification).
//!
//! Module layout (built in order): `schema` (seeded tables/columns),
//! `oracle` (rusqlite reference + DDL translation + error-class mapping +
//! discard rules), then `generator`, `wire`, `compare`, `shrink`, and
//! `scenarios`. The self-tests gate the verifier itself: oracle-vs-oracle
//! agreement, mutant toy engines, profile-coverage assertions and
//! per-stage comparable-check floors.

pub mod compare;
pub mod generator;
pub mod oracle;
pub mod schema;
pub mod wire;
