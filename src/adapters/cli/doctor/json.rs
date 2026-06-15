//! JSON output DTOs for `rustain doctor --json` (Story 13.2, AC9).
//!
//! Boundary coercion: domain `CheckResult` → snake_case `CheckResultOut`.
//! Mirrors the `ask.rs` `From<&Domain>` DTO pattern (Story 13.1b).

use serde::Serialize;

use super::{CheckResult, CheckStatus};

pub const DOCTOR_SCHEMA_VERSION: &str = "1.2";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DoctorReport {
    pub schema_version: String,
    pub checks: Vec<CheckResultOut>,
    pub summary: DoctorSummary,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CheckResultOut {
    pub name: String,
    pub category: String,
    /// One of: pass, warning, fail, skipped
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DoctorSummary {
    pub passed: usize,
    pub info: usize,
    pub warnings: usize,
    pub failures: usize,
    pub skipped: usize,
    pub total: usize,
}

impl From<&CheckResult> for CheckResultOut {
    fn from(r: &CheckResult) -> Self {
        let status = match &r.status {
            CheckStatus::Pass => "pass",
            CheckStatus::Info => "info",
            CheckStatus::Warning => "warning",
            CheckStatus::Fail => "fail",
            CheckStatus::Skipped(_) => "skipped",
        };
        Self {
            name: r.name.clone(),
            category: r.category.clone(),
            status: status.to_string(),
            message: r.message.clone(),
            fix: r.fix.clone(),
            latency_ms: r.latency.map(|d| d.as_millis() as u64),
        }
    }
}

impl DoctorReport {
    /// Build a `DoctorReport` from a slice of domain `CheckResult`s.
    pub fn from_results(results: &[CheckResult]) -> Self {
        let checks: Vec<CheckResultOut> = results.iter().map(CheckResultOut::from).collect();
        let passed = results
            .iter()
            .filter(|r| r.status == CheckStatus::Pass)
            .count();
        let info = results
            .iter()
            .filter(|r| r.status == CheckStatus::Info)
            .count();
        let warnings = results
            .iter()
            .filter(|r| r.status == CheckStatus::Warning)
            .count();
        let failures = results
            .iter()
            .filter(|r| r.status == CheckStatus::Fail)
            .count();
        let skipped = results.iter().filter(|r| r.status.is_skipped()).count();
        Self {
            schema_version: DOCTOR_SCHEMA_VERSION.to_string(),
            checks,
            summary: DoctorSummary {
                passed,
                info,
                warnings,
                failures,
                skipped,
                total: results.len(),
            },
        }
    }
}
