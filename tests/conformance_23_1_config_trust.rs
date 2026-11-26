//! Story 23.1 review H1: `[sandbox]` from a checkout's own config layers can
//! only tighten the sandbox. The production layer assembly
//! (`load_from_explicit_layers`) with every layer explicit.

use clap::Parser as _;
use rustain::adapters::cli::commands::Cli;
use rustain::adapters::cli::config_cmd::LayerDescriptor;
use rustain::adapters::profile_resolver::noop::NoopProfileResolver;
use rustain::domain::models::AppConfig;
use rustain::infrastructure::config::{
    ACTIVE_PROFILE_PRIORITY, BUILT_IN_DEFAULTS_PRIORITY, CLI_FLAGS_PRIORITY, ENV_VARS_PRIORITY,
    LOCAL_OVERRIDE_PRIORITY, USER_GLOBAL_CONFIG_PRIORITY, WORKSPACE_CONFIG_PRIORITY,
    load_from_explicit_layers, parse_config_overrides,
};
use serde_json::json;

struct Loaded {
    config: AppConfig,
    workspace: String,
    user: String,
}

fn load(user: Option<&str>, workspace: Option<&str>, overrides: &[&str]) -> Loaded {
    let dir = tempfile::tempdir().expect("scratch dir");
    let file = |name: &str, body: Option<&str>| {
        body.map(|body| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).expect("write the layer file");
            path
        })
    };
    let (user_path, workspace_path) = (file("user.toml", user), file("workspace.toml", workspace));
    let layer = |kind, path: &Option<std::path::PathBuf>, priority| LayerDescriptor {
        kind,
        path: path.clone(),
        exists: path.is_some(),
        priority,
    };
    let layers = [
        layer("CLI flags", &None, CLI_FLAGS_PRIORITY),
        layer("env", &None, ENV_VARS_PRIORITY),
        layer("local", &None, LOCAL_OVERRIDE_PRIORITY),
        layer("workspace", &workspace_path, WORKSPACE_CONFIG_PRIORITY),
        layer("user-global", &user_path, USER_GLOBAL_CONFIG_PRIORITY),
        layer("profile", &None, ACTIVE_PROFILE_PRIORITY),
        layer("defaults", &None, BUILT_IN_DEFAULTS_PRIORITY),
    ];
    let cli = Cli::parse_from(["rustain"]);
    let pairs: Vec<String> = overrides.iter().map(ToString::to_string).collect();
    let parsed = parse_config_overrides(&pairs).expect("overrides parse");
    let config = load_from_explicit_layers(
        &cli,
        &layers,
        &NoopProfileResolver,
        Some(&parsed),
        &json!({}),
    )
    .expect("config loads");
    Loaded {
        config,
        workspace: workspace_path
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        user: user_path
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}

/// A workspace layer cannot switch the sandbox off, re-enable the network or
/// widen the writable set; each attempt is ignored and named.
#[test]
fn h1_workspace_layer_cannot_weaken_the_sandbox() {
    let loaded = load(
        Some("[sandbox]\nnetwork = false\n"),
        Some(
            "[sandbox]\nadapter = \"noop\"\nnetwork = true\nwritable_roots = [\"~\"]\n\
             deny_read = [\"~/.aws\"]\n",
        ),
        &[],
    );
    let sandbox = &loaded.config.sandbox;
    assert_eq!(sandbox.adapter, "auto");
    assert!(!sandbox.network, "the user's network = false stands");
    assert!(sandbox.writable_roots.is_empty());
    assert_eq!(sandbox.deny_read, vec![std::path::PathBuf::from("~/.aws")]);
    let ignored = &sandbox.provenance.ignored;
    assert_eq!(ignored.len(), 3, "{ignored:?}");
    for key in ["adapter = \"noop\"", "network = true", "writable_roots"] {
        assert!(
            ignored
                .iter()
                .any(|m| m.contains(key) && m.contains(&loaded.workspace)),
            "{key}: {ignored:?}"
        );
    }
    assert_eq!(sandbox.provenance.adapter_source, "built-in default");
}

/// Tightening from a workspace layer applies and is attributed to the file.
#[test]
fn h1_workspace_layer_may_tighten_the_sandbox() {
    let loaded = load(
        None,
        Some("[sandbox]\nadapter = \"landlock\"\nnetwork = false\n"),
        &[],
    );
    let sandbox = &loaded.config.sandbox;
    assert_eq!(sandbox.adapter, "landlock");
    assert!(!sandbox.network);
    assert!(sandbox.provenance.ignored.is_empty());
    assert_eq!(sandbox.provenance.adapter_source, loaded.workspace);
}

/// The trusted layers still decide: the user file and `-c` may switch the
/// sandbox off and widen it.
#[test]
fn h1_user_global_and_overrides_are_honored() {
    let loaded = load(
        Some("[sandbox]\nadapter = \"noop\"\nwritable_roots = [\"/opt/tool\"]\n"),
        None,
        &[],
    );
    assert_eq!(loaded.config.sandbox.adapter, "noop");
    assert_eq!(
        loaded.config.sandbox.writable_roots,
        vec![std::path::PathBuf::from("/opt/tool")]
    );
    assert_eq!(loaded.config.sandbox.provenance.adapter_source, loaded.user);

    let loaded = load(None, None, &["sandbox.adapter=noop"]);
    assert_eq!(loaded.config.sandbox.adapter, "noop");
    assert!(loaded.config.sandbox.provenance.ignored.is_empty());
}
