use std::collections::BTreeMap;

use crate::domain::models::PortDimension;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapTier {
    Hot,
    Warm,
    Cold,
}

pub fn swap_tier(port: PortDimension) -> SwapTier {
    match port {
        PortDimension::Persona | PortDimension::Tools | PortDimension::Context => SwapTier::Hot,
        PortDimension::Memory | PortDimension::Session => SwapTier::Warm,
        PortDimension::Channels | PortDimension::Scheduler => SwapTier::Cold,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapPolicy {
    CarryOver,
    FreshStart,
    // TODO Story 8.4-FU1 — falls through to CarryOver until real adapters land
    Merge,
    // TODO Story 8.4-FU1 — falls through to CarryOver until real adapters land
    Selective,
}

impl SwapPolicy {
    pub const fn default_for(_port: PortDimension) -> Self {
        Self::CarryOver
    }
}

#[derive(Debug, Clone)]
pub struct TransitionPlan {
    pub profile_name: String,
    pub identity_color: u8,
    pub diffs: Vec<PortDiff>,
    pub estimated_ms: u64,
}

#[derive(Debug, Clone)]
pub struct PortDiff {
    pub port: PortDimension,
    pub tier: SwapTier,
    pub from_adapter: String,
    pub to_adapter: String,
    pub policy: SwapPolicy,
}

impl PortDiff {
    pub fn port_name(&self) -> &'static str {
        match self.port {
            PortDimension::Persona => "persona",
            PortDimension::Memory => "memory",
            PortDimension::Session => "session",
            PortDimension::Tools => "tools",
            PortDimension::Channels => "channels",
            PortDimension::Scheduler => "scheduler",
            PortDimension::Context => "context",
        }
    }
}

impl TransitionPlan {
    pub fn from_selections(
        current: &BTreeMap<PortDimension, crate::domain::models::AdapterRef>,
        target: &BTreeMap<PortDimension, crate::domain::models::AdapterRef>,
        profile_name: &str,
        identity_color: u8,
    ) -> Self {
        let mut diffs = Vec::new();

        let all_keys: std::collections::BTreeSet<PortDimension> = current
            .keys()
            .chain(target.keys())
            .copied()
            .collect();

        for &port in &all_keys {
            let from_adapter = current
                .get(&port)
                .map(|a| a.adapter.clone())
                .unwrap_or_else(|| "none".to_string());
            let to_adapter = target
                .get(&port)
                .map(|a| a.adapter.clone())
                .unwrap_or_else(|| "none".to_string());

            if from_adapter != to_adapter {
                diffs.push(PortDiff {
                    port,
                    tier: swap_tier(port),
                    from_adapter,
                    to_adapter,
                    policy: SwapPolicy::default_for(port),
                });
            }
        }

        let any_hot = diffs.iter().any(|d| d.tier == SwapTier::Hot);
        let any_warm = diffs.iter().any(|d| d.tier == SwapTier::Warm);
        let any_cold = diffs.iter().any(|d| d.tier == SwapTier::Cold);

        let estimated_ms = (if any_hot { 10 } else { 0 })
            + (if any_warm { 5000 } else { 0 })
            + (if any_cold { 2000 } else { 0 });

        Self {
            profile_name: profile_name.to_string(),
            identity_color,
            diffs,
            estimated_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_swap_tier_persona_hot() {
        assert_eq!(swap_tier(PortDimension::Persona), SwapTier::Hot);
    }

    #[test]
    fn test_swap_tier_tools_hot() {
        assert_eq!(swap_tier(PortDimension::Tools), SwapTier::Hot);
    }

    #[test]
    fn test_swap_tier_context_hot() {
        assert_eq!(swap_tier(PortDimension::Context), SwapTier::Hot);
    }

    #[test]
    fn test_swap_tier_memory_warm() {
        assert_eq!(swap_tier(PortDimension::Memory), SwapTier::Warm);
    }

    #[test]
    fn test_swap_tier_session_warm() {
        assert_eq!(swap_tier(PortDimension::Session), SwapTier::Warm);
    }

    #[test]
    fn test_swap_tier_channels_cold() {
        assert_eq!(swap_tier(PortDimension::Channels), SwapTier::Cold);
    }

    #[test]
    fn test_swap_tier_scheduler_cold() {
        assert_eq!(swap_tier(PortDimension::Scheduler), SwapTier::Cold);
    }

    #[test]
    fn test_transition_plan_no_diffs_when_selections_equal() {
        let dims_a = BTreeMap::from([
            (PortDimension::Persona, crate::domain::models::AdapterRef { adapter: "coding".into(), _config: None }),
            (PortDimension::Memory, crate::domain::models::AdapterRef { adapter: "project-scoped".into(), _config: None }),
        ]);
        let plan = TransitionPlan::from_selections(&dims_a, &dims_a, "test", 5);
        assert!(plan.diffs.is_empty());
        assert_eq!(plan.estimated_ms, 0);
    }

    #[test]
    fn test_transition_plan_diff_classification() {
        let current = BTreeMap::from([
            (PortDimension::Persona, crate::domain::models::AdapterRef { adapter: "old".into(), _config: None }),
            (PortDimension::Memory, crate::domain::models::AdapterRef { adapter: "old".into(), _config: None }),
            (PortDimension::Channels, crate::domain::models::AdapterRef { adapter: "old".into(), _config: None }),
            (PortDimension::Scheduler, crate::domain::models::AdapterRef { adapter: "cron".into(), _config: None }),
        ]);
        let target = BTreeMap::from([
            (PortDimension::Persona, crate::domain::models::AdapterRef { adapter: "new".into(), _config: None }),
            (PortDimension::Memory, crate::domain::models::AdapterRef { adapter: "new".into(), _config: None }),
            (PortDimension::Channels, crate::domain::models::AdapterRef { adapter: "new".into(), _config: None }),
        ]);
        let plan = TransitionPlan::from_selections(&current, &target, "profile", 6);
        assert_eq!(plan.diffs.len(), 4);
        assert!(plan.diffs.iter().any(|d| d.port == PortDimension::Persona && d.tier == SwapTier::Hot));
        assert!(plan.diffs.iter().any(|d| d.port == PortDimension::Memory && d.tier == SwapTier::Warm));
        assert!(plan.diffs.iter().any(|d| d.port == PortDimension::Channels && d.tier == SwapTier::Cold));
        assert!(plan.diffs.iter().any(|d| d.port == PortDimension::Scheduler
            && d.from_adapter == "cron"
            && d.to_adapter == "none"));
    }

    #[test]
    fn test_swap_policy_default_for_returns_carryover() {
        assert_eq!(SwapPolicy::default_for(PortDimension::Persona), SwapPolicy::CarryOver);
        assert_eq!(SwapPolicy::default_for(PortDimension::Memory), SwapPolicy::CarryOver);
        assert_eq!(SwapPolicy::default_for(PortDimension::Channels), SwapPolicy::CarryOver);
    }
}
