//! Adapter health snapshot — sync, allocation-light read of per-port adapter state.
//! Pulled per render tick by the Adapter Status panel (Story 8.5 AC-2..AC-4).
//! Default impl returns `unknown()`; real adapters land in Epic 12.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum HealthLevel {
    Healthy,
    Degraded,
    Error,
    Unknown,
}

impl HealthLevel {
    pub fn symbol(&self) -> char {
        use crate::domain::models::visual::symbols::*;
        match self {
            Self::Healthy => SUCCESS,
            Self::Degraded => WARNING,
            Self::Error => ERROR,
            Self::Unknown => UNKNOWN,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthSummary {
    pub level: HealthLevel,
    pub metric: String,
    pub suggested_action: Option<&'static str>,
}

impl HealthSummary {
    pub fn healthy(metric: impl Into<String>) -> Self {
        Self {
            level: HealthLevel::Healthy,
            metric: metric.into(),
            suggested_action: None,
        }
    }
    pub fn degraded(metric: impl Into<String>, action: &'static str) -> Self {
        Self {
            level: HealthLevel::Degraded,
            metric: metric.into(),
            suggested_action: Some(action),
        }
    }
    pub fn error(metric: impl Into<String>, action: &'static str) -> Self {
        Self {
            level: HealthLevel::Error,
            metric: metric.into(),
            suggested_action: Some(action),
        }
    }
    pub fn unknown() -> Self {
        Self {
            level: HealthLevel::Unknown,
            metric: String::from("n/a"),
            suggested_action: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unknown_constructor() {
        let s = HealthSummary::unknown();
        assert_eq!(s.level, HealthLevel::Unknown);
        assert_eq!(s.metric, "n/a");
        assert!(s.suggested_action.is_none());
    }

    #[test]
    fn test_healthy_no_action() {
        let s = HealthSummary::healthy("entries: 42");
        assert_eq!(s.level, HealthLevel::Healthy);
        assert!(s.suggested_action.is_none());
    }

    #[test]
    fn test_error_with_action() {
        let s = HealthSummary::error("write failed", "check workspace permissions");
        assert_eq!(s.level, HealthLevel::Error);
        assert_eq!(s.suggested_action, Some("check workspace permissions"));
    }

    #[test]
    fn test_symbol_uniqueness() {
        assert_ne!(HealthLevel::Healthy.symbol(), HealthLevel::Degraded.symbol());
        assert_ne!(HealthLevel::Healthy.symbol(), HealthLevel::Error.symbol());
        assert_ne!(HealthLevel::Healthy.symbol(), HealthLevel::Unknown.symbol());
        assert_ne!(HealthLevel::Degraded.symbol(), HealthLevel::Error.symbol());
        assert_ne!(HealthLevel::Degraded.symbol(), HealthLevel::Unknown.symbol());
        assert_ne!(HealthLevel::Error.symbol(), HealthLevel::Unknown.symbol());
    }
}
