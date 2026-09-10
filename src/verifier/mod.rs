pub mod compliance;
pub mod dns;
pub mod http;
pub mod pdf;
pub mod redis;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageResult {
    pub stage: u32,
    pub name: String,
    pub passed: bool,
    pub error: Option<String>,
}

pub use compliance::ComplianceChecker;
pub use dns::DnsVerifier;
pub use http::HttpVerifier;
pub use pdf::PdfVerifier;
pub use redis::RedisVerifier;
