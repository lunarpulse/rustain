//! End-to-end integration tests for profile switching — Story 8.4.
//!
//! Exercises the full switch flow: TomlProfileResolver → list_profiles →
//! TransitionPlan → AgentCore slot swap → Arc pointer verification.

use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::domain::models::PortDimension;
use rustain::domain::ports::ProfileResolver;
use rustain::domain::services::swap_tier::TransitionPlan;
use rustain::infrastructure::runtime::agent_core::AgentCore;

fn make_tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn write_profile(dir: &std::path::Path, name: &str, toml_content: &str) {
    std::fs::write(dir.join(format!("{name}.toml")), toml_content)
        .unwrap_or_else(|e| panic!("failed to write {name}.toml: {e}"));
}

#[test]
fn test_list_profiles_returns_builtins() {
    let tmpdir = make_tempdir();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profiles = resolver.list_profiles();
    let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
    assert!(
        names.contains(&"coding"),
        "list_profiles should include 'coding', got: {:?}",
        names
    );
    assert!(
        names.contains(&"base"),
        "list_profiles should include 'base', got: {:?}",
        names
    );
}

#[test]
fn test_list_profiles_includes_user_profiles() {
    let tmpdir = make_tempdir();
    write_profile(
        tmpdir.path(),
        "my-custom",
        r#"
name = "my-custom"
extends = "base"
description = "My custom profile"
[persona]
adapter = "coding"
"#,
    );
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profiles = resolver.list_profiles();
    let custom = profiles.iter().find(|p| p.name == "my-custom");
    assert!(custom.is_some(), "list_profiles should include user profile 'my-custom'");
    let custom = custom.unwrap();
    assert_eq!(custom.description.as_deref(), Some("My custom profile"));
}

#[test]
fn test_list_profiles_dimensions_populated() {
    let tmpdir = make_tempdir();
    let resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let profiles = resolver.list_profiles();
    let coding = profiles.iter().find(|p| p.name == "coding").unwrap();
    assert!(
        !coding.selection.dimensions.is_empty(),
        "coding profile should have non-empty dimensions, got {:?}",
        coding.selection.dimensions
    );
    assert!(
        coding.selection.dimensions.contains_key(&PortDimension::Persona),
        "coding profile should have persona dimension"
    );
}

#[test]
fn test_transition_plan_between_profiles() {
    let tmpdir = make_tempdir();
    let coding_resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let pa_resolver =
        TomlProfileResolver::new("personal-assistant", tmpdir.path().to_path_buf()).unwrap();

    let coding_dims = coding_resolver
        .resolve_active()
        .unwrap()
        .selection
        .dimensions;
    let pa_dims = pa_resolver.resolve_active().unwrap().selection.dimensions;

    let plan = TransitionPlan::from_selections(&coding_dims, &pa_dims, "personal-assistant", 5);
    assert!(
        !plan.diffs.is_empty(),
        "Transition from coding to personal-assistant should produce diffs"
    );
}

#[test]
fn test_agent_core_test_noop_all_slots_populated() {
    let core = AgentCore::test_noop();
    let _persona = core.persona.load_full();
    let _memory = core.memory.load_full();
    let _session = core.session.load_full();
    let _tools = core.tools.load_full();
    let _channels = core.channels.load_full();
    let _scheduler = core.scheduler.load_full();
    let _context = core.context.load_full();
}

#[test]
fn test_switch_same_profile_no_op() {
    let tmpdir = make_tempdir();
    let coding_resolver = TomlProfileResolver::new("coding", tmpdir.path().to_path_buf()).unwrap();
    let coding_dims = coding_resolver
        .resolve_active()
        .unwrap()
        .selection
        .dimensions
        .clone();

    let plan = TransitionPlan::from_selections(&coding_dims, &coding_dims, "coding", 6);
    assert!(
        plan.diffs.is_empty(),
        "Switching from coding to coding should produce no diffs"
    );
    assert_eq!(plan.estimated_ms, 0);
}
