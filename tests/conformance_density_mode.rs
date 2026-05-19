//! Conformance tests for information density modes — Story 8.4b AC-12.
//!
//! 5 ratchets:
//! 1. Domain isolation — no adapter/infra imports in domain/visual.rs DensityMode
//! 2. DensityMode::default() == Focus
//! 3. indicator_chars are unique (F/M/D)
//! 4. serde roundtrip (lowercase TOML/JSON)
//! 5. only Monitor defaults sidebar to visible

use rustain::domain::models::visual::DensityMode;

#[test]
fn test_density_mode_domain_isolation() {
    let forbidden = ["use crate::adapters", "use crate::infrastructure"];
    let domain_module = include_str!("../src/domain/models/visual.rs");
    for pattern in &forbidden {
        assert!(
            !domain_module.contains(pattern),
            "src/domain/models/visual.rs must not import from {}",
            pattern
        );
    }
}

#[test]
fn test_density_mode_default_is_focus() {
    assert_eq!(DensityMode::default(), DensityMode::Focus);
}

#[test]
fn test_density_mode_indicator_chars_unique() {
    let f = DensityMode::Focus.indicator_char();
    let m = DensityMode::Monitor.indicator_char();
    let d = DensityMode::Dashboard.indicator_char();
    assert_eq!(f, 'F');
    assert_eq!(m, 'M');
    assert_eq!(d, 'D');
    assert_ne!(f, m);
    assert_ne!(f, d);
    assert_ne!(m, d);
}

#[test]
fn test_density_mode_serde_roundtrip() {
    let cases: &[(&str, DensityMode)] = &[
        ("\"focus\"", DensityMode::Focus),
        ("\"monitor\"", DensityMode::Monitor),
        ("\"dashboard\"", DensityMode::Dashboard),
    ];
    for (input, expected) in cases {
        let parsed: DensityMode = serde_json::from_str(input).unwrap();
        assert_eq!(parsed, *expected);
        let serialized = serde_json::to_string(expected).unwrap();
        assert_eq!(serialized, *input);
    }
}

#[test]
fn test_density_mode_sidebar_default_only_monitor_true() {
    assert!(!DensityMode::Focus.default_sidebar_visible());
    assert!(DensityMode::Monitor.default_sidebar_visible());
    assert!(!DensityMode::Dashboard.default_sidebar_visible());
}
