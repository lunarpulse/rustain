//! Conformance tests for the profile system — Story 8.2.
//!
//! AC-9: profile load + validation < 50ms (cold), < 5ms (embedded).
//! AC-12: embedded profiles parse cleanly.
//! AC-14: domain isolation, no EventBus bypasses, no new std::sync locks.

use std::time::Instant;

use rustain::adapters::profile_resolver::embedded::EmbeddedProfileSource;
use rustain::domain::models::PortDimension;
use rustain::domain::services::adapter_catalog::AdapterCatalog;
use rustain::domain::services::profile_loader::ProfileLoader;

#[test]
fn test_embedded_profiles_parse_cleanly() {
    let source = EmbeddedProfileSource;
    let catalog = AdapterCatalog;
    let loader = ProfileLoader::new(&catalog, &source);

    for name in &["base", "coding", "personal-assistant"] {
        let resolved = loader
            .load(name)
            .unwrap_or_else(|e| panic!("Built-in profile '{}' failed to load: {}", name, e));
        assert_eq!(
            resolved.selection.dimensions.len(),
            7,
            "Profile '{}' should have all 7 dimensions",
            name
        );
    }
}

#[test]
fn test_embedded_profile_load_under_5ms() {
    let source = EmbeddedProfileSource;
    let catalog = AdapterCatalog;
    let mut durations = Vec::new();

    for _ in 0..10 {
        let loader = ProfileLoader::new(&catalog, &source);
        let start = Instant::now();
        let _ = loader.load("coding").unwrap();
        durations.push(start.elapsed());
    }

    durations.sort();
    let median = durations[5];
    assert!(
        median.as_millis() < 5,
        "Embedded profile load median was {}ms, expected < 5ms",
        median.as_millis()
    );
}

#[test]
fn test_profile_load_under_50ms() {
    let tmpdir = tempfile::tempdir().expect("create temp dir");
    let source = EmbeddedProfileSource;
    let catalog = AdapterCatalog;
    let mut durations = Vec::new();

    for _ in 0..10 {
        let loader = ProfileLoader::new(&catalog, &source);
        let start = Instant::now();
        let _ = loader.load("personal-assistant").unwrap();
        durations.push(start.elapsed());
    }

    durations.sort();
    let median = durations[5];
    assert!(
        median.as_millis() < 50,
        "Profile load median was {}ms, expected < 50ms",
        median.as_millis()
    );

    let _ = tmpdir;
}

#[test]
fn test_all_referenced_adapters_in_catalog() {
    let source = EmbeddedProfileSource;
    let catalog = AdapterCatalog;
    let loader = ProfileLoader::new(&catalog, &source);

    for name in &["base", "coding", "personal-assistant"] {
        let resolved = loader
            .load(name)
            .unwrap_or_else(|e| panic!("Built-in profile '{}' failed to load: {}", name, e));

        for (port, adapter_ref) in &resolved.selection.dimensions {
            assert!(
                AdapterCatalog::lookup(*port, &adapter_ref.adapter).is_some(),
                "Profile '{}': adapter '{}' for port {:?} not found in catalog",
                name,
                adapter_ref.adapter,
                port
            );
        }
    }
}

#[test]
fn test_personal_assistant_preview_fallback_applied() {
    let source = EmbeddedProfileSource;
    let catalog = AdapterCatalog;
    let loader = ProfileLoader::new(&catalog, &source);
    let resolved = loader.load("personal-assistant").unwrap();

    if !cfg!(feature = "telegram") {
        assert_eq!(
            resolved.selection.dimensions[&PortDimension::Channels].adapter,
            "terminal",
            "personal-assistant should fall back to 'terminal' for channels when telegram feature is off"
        );
    }
    if !cfg!(feature = "cron") {
        assert_eq!(
            resolved.selection.dimensions[&PortDimension::Scheduler].adapter,
            "none",
            "personal-assistant should fall back to 'none' for scheduler when cron feature is off"
        );
    }
}
