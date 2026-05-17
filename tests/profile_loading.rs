//! Integration tests for profile loading — Story 8.2.
//!
//! Exercises TomlProfileResolver end-to-end with temp directories,
//! custom profiles, embedded fallback, and reload behavior.

use std::sync::Arc;

use arc_swap::ArcSwap;

use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::domain::models::PortDimension;
use rustain::domain::ports::ProfileResolver;

fn full_base_toml() -> &'static str {
    r#"name = "base"
[persona]
adapter = "minimal"
[memory]
adapter = "noop"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#
}

#[test]
fn load_coding_from_embedded() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(profile.name, "coding");
    assert!(profile.selection.dimensions.contains_key(&PortDimension::Persona));
    assert_eq!(profile.selection.dimensions[&PortDimension::Persona].adapter, "coding");
}

#[test]
fn load_personal_assistant_from_embedded() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver =
        TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(profile.name, "personal-assistant");
    assert!(profile.preview);
}

#[test]
fn custom_profile_extends_coding() {
    let tmpdir = tempfile::tempdir().unwrap();
    let custom = r#"
name = "my-dev"
extends = "coding"
[memory]
adapter = "daily-log"
"#;
    std::fs::write(tmpdir.path().join("my-dev.toml"), custom).unwrap();
    let resolver = TomlProfileResolver::new("my-dev", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(profile.name, "my-dev");
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Memory].adapter,
        "daily-log"
    );
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Persona].adapter,
        "coding"
    );
}

#[test]
fn custom_coding_overrides_embedded() {
    let tmpdir = tempfile::tempdir().unwrap();
    let custom = r#"
name = "coding"
extends = "base"
[persona]
adapter = "personal-assistant"
[memory]
adapter = "daily-log"
[session]
adapter = "basic"
[tools]
adapter = "builtin-only"
[channels]
adapter = "terminal"
[scheduler]
adapter = "none"
[context]
adapter = "default"
"#;
    std::fs::write(tmpdir.path().join("coding.toml"), custom).unwrap();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profile = resolver.resolve_active().unwrap();
    assert_eq!(
        profile.selection.dimensions[&PortDimension::Persona].adapter,
        "personal-assistant"
    );
}

#[test]
fn nonexistent_profile_returns_error() {
    let tmpdir = tempfile::tempdir().unwrap();
    let result = TomlProfileResolver::new("no-such-profile", tmpdir.path().to_path_buf());
    assert!(result.is_err());
}

#[test]
fn reload_swaps_profile_resolver() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver1 = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let swap: Arc<ArcSwap<Arc<dyn ProfileResolver>>> = Arc::new(ArcSwap::from_pointee(
        Arc::new(resolver1) as Arc<dyn ProfileResolver>,
    ));

    let profile1 = swap.load_full().resolve_active().unwrap();
    assert_eq!(profile1.name, "coding");

    let resolver2 = TomlProfileResolver::new("base", tmpdir.path().to_path_buf()).unwrap();
    swap.store(Arc::new(Arc::new(resolver2) as Arc<dyn ProfileResolver>));

    let profile2 = swap.load_full().resolve_active().unwrap();
    assert_eq!(profile2.name, "base");
}

#[test]
fn preview_warning_emitted_once() {
    let tmpdir = tempfile::tempdir().unwrap();
    let resolver =
        TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();
    assert!(resolver.take_preview_warning().is_some());
    assert!(resolver.take_preview_warning().is_none());
}
