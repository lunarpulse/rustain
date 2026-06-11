//! Config reload integration tests — Story 8.1 AC-7.
//!
//! AC-7: In-flight streaming turn observes pre-reload config; next turn
//! observes the reloaded config. Uses ArcSwap direct manipulation to avoid
//! needing the full signal/CLI reload path.

use std::sync::Arc;

use arc_swap::ArcSwap;
use rustain::domain::models::AppConfig;

/// AC-7: In-flight snapshot captured at spawn time survives a concurrent
/// ArcSwap store — the spawned task continues to read its captured Arc.
#[test]
fn test_in_flight_snapshot_survives_reload() {
    let original = AppConfig {
        model: "original-model".to_string(),
        ..AppConfig::default()
    };
    let swapped = AppConfig {
        model: "swapped-model".to_string(),
        ..AppConfig::default()
    };

    let swap = Arc::new(ArcSwap::from_pointee(original.clone()));

    // Simulate a spawned task capturing a snapshot before reload
    let captured = swap.load_full();
    assert_eq!(captured.model, "original-model");

    // Reload: store the new config
    swap.store(Arc::new(swapped));

    // The captured snapshot still observes the original model
    assert_eq!(captured.model, "original-model");

    // A fresh load observes the new model
    let fresh = swap.load_full();
    assert_eq!(fresh.model, "swapped-model");
}

/// AC-7: After the event loop re-snaps config (simulated here as a second
/// load_full()), synchronous reads pick up the reloaded config.
#[test]
fn test_re_snap_after_reload_picks_up_new_config() {
    let original = AppConfig {
        model: "before-reload".to_string(),
        ..AppConfig::default()
    };
    let reloaded = AppConfig {
        model: "after-reload".to_string(),
        ..AppConfig::default()
    };

    let swap = Arc::new(ArcSwap::from_pointee(original));

    let config_arc = swap.load_full();
    assert_eq!(config_arc.model, "before-reload");

    // Reload happens
    swap.store(Arc::new(reloaded));

    // Re-snap (what the ConfigReload dispatch arm does)
    let config_arc = swap.load_full();
    assert_eq!(config_arc.model, "after-reload");
}
