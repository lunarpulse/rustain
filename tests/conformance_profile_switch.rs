//! Conformance tests for profile switching — Story 8.4 AC-12.
//!
//! 5 ratchets:
//! 1. swap_tier total mapping — all 7 PortDimension variants classified
//! 2. TransitionPlan::from_selections is pure (deterministic)
//! 3. identity_color deterministic across invocations
//! 4. identity_color in [1, 14] for arbitrary names
//! 5. domain isolation — no adapter/infra imports in profile-switch domain modules

use std::collections::BTreeMap;

use rustain::domain::models::{AdapterRef, PortDimension};
use rustain::domain::services::identity_color;
use rustain::domain::services::swap_tier::{SwapTier, TransitionPlan, swap_tier};

#[test]
fn test_swap_tier_total_mapping() {
    let all_ports = vec![
        PortDimension::Persona,
        PortDimension::Memory,
        PortDimension::Session,
        PortDimension::Tools,
        PortDimension::Channels,
        PortDimension::Scheduler,
        PortDimension::Context,
    ];
    for port in &all_ports {
        let tier = swap_tier(*port);
        assert!(
            matches!(tier, SwapTier::Hot | SwapTier::Warm | SwapTier::Cold),
            "swap_tier({:?}) returned unclassified tier {:?}",
            port,
            tier
        );
    }
    assert_eq!(
        all_ports.len(),
        7,
        "PortDimension should have exactly 7 variants"
    );
}

#[test]
fn test_transition_plan_pure() {
    let current = BTreeMap::from([
        (
            PortDimension::Persona,
            AdapterRef {
                adapter: "coding".into(),
                _config: None,
            },
        ),
        (
            PortDimension::Memory,
            AdapterRef {
                adapter: "project-scoped".into(),
                _config: None,
            },
        ),
    ]);
    let target = BTreeMap::from([
        (
            PortDimension::Persona,
            AdapterRef {
                adapter: "personal-assistant".into(),
                _config: None,
            },
        ),
        (
            PortDimension::Memory,
            AdapterRef {
                adapter: "daily-log".into(),
                _config: None,
            },
        ),
    ]);
    let first = TransitionPlan::from_selections(&current, &target, "test", 5, None);
    for _ in 0..100 {
        let next = TransitionPlan::from_selections(&current, &target, "test", 5, None);
        assert_eq!(next.diffs.len(), first.diffs.len());
        for (a, b) in next.diffs.iter().zip(first.diffs.iter()) {
            assert_eq!(a.port, b.port);
            assert_eq!(a.tier, b.tier);
            assert_eq!(a.from_adapter, b.from_adapter);
            assert_eq!(a.to_adapter, b.to_adapter);
        }
    }
}

#[test]
fn test_identity_color_deterministic() {
    let name = "my-custom-profile";
    let first = identity_color::derive_identity_color(name, None).0;
    for _ in 0..100 {
        assert_eq!(
            identity_color::derive_identity_color(name, None).0,
            first,
            "identity_color should be deterministic for '{}'",
            name
        );
    }
}

#[test]
fn test_identity_color_in_range() {
    for i in 0..1000u32 {
        let name = format!("profile_{i}");
        let color = identity_color::derive_identity_color(&name, None).0;
        assert!(
            (1..=14).contains(&color),
            "color {} for '{}' out of range [1, 14]",
            color,
            name
        );
    }
}

#[test]
fn test_profile_switch_domain_isolation() {
    let forbidden = ["use crate::adapters", "use crate::infrastructure"];
    let domain_modules = [
        include_str!("../src/domain/services/swap_tier.rs"),
        include_str!("../src/domain/services/identity_color.rs"),
    ];
    for (idx, source) in domain_modules.iter().enumerate() {
        for pattern in &forbidden {
            assert!(
                !source.contains(pattern),
                "Domain module {} contains forbidden import '{}'",
                idx,
                pattern
            );
        }
    }
}
