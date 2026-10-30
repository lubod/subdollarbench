pub mod compliance;
pub mod sql;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageResult {
    pub stage: u32,
    pub name: String,
    pub passed: bool,
    pub error: Option<String>,
}

pub use compliance::ComplianceChecker;

/// Helper to extract an 8-character hex seed prefix from an optional run seed.
pub fn get_seed_prefix<'a>(run_seed: Option<&'a str>, default_val: &'a str) -> &'a str {
    match run_seed {
        Some(s) if !s.is_empty() => {
            if s.len() <= 8 {
                s
            } else {
                &s[s.len() - 8..]
            }
        }
        _ => default_val,
    }
}
