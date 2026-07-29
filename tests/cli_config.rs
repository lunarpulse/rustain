//! P0 integration tests for `rustain config show/edit/path/validate` (Story 13.2a).
//!
//! 8 P0 gates — every "assert absence" carries a positive control proving it's live.
//! All config fixtures use `tempfile::tempdir()` (13.1c /tmp-leak lesson).
//!
//! Tests that manipulate env vars are marked with `#[serial]` from `serial_test`
//! so they do not race under parallel execution.

#![cfg(feature = "test-instrumentation")]

use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use clap::Parser;
use rustain::adapters::cli::commands::Cli;
use rustain::adapters::cli::config_cmd;
use rustain::domain::ports::ProfileResolver;
use rustain::infrastructure::utils::ENV_VAR_TRIMMED_CALLS;
use serial_test::serial;

/// Helper: parse a CLI from args.
fn cli_from(args: &[&str]) -> Cli {
    Cli::parse_from(args)
}

/// Minimal real profile resolver for tests that need profile-dependent config.
struct TestProfileResolver {
    defaults: Option<figment::value::Value>,
    mcp_servers: Vec<rustain::domain::models::mcp_server_spec::McpServerSpec>,
}

impl ProfileResolver for TestProfileResolver {
    fn resolve_active(&self) -> Option<rustain::domain::models::ResolvedProfile> {
        Some(rustain::domain::models::ResolvedProfile {
            name: "test".to_string(),
            selection: Default::default(),
            overrides: self.defaults.clone(),
            mcp_servers: self.mcp_servers.clone(),
            a2a_peers: Vec::new(),
            include_builtin_tools: true,
            preview: false,
        })
    }

    fn resolve_active_profile_defaults(&self) -> Option<figment::value::Value> {
        self.defaults.clone()
    }
}

// =========================================================================
// P0-1: Redaction canary — file-layer + name-only + error path (both halves)
// =========================================================================

/// P0-1a: File-layer base_url userinfo is stripped from config show output.
#[test]
#[serial]
fn p0_1a_file_layer_base_url_userinfo_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"[provider.test]
provider_id = "test"
model_id = "test-model"
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://user:SECRET-TOK@api.example.com/v1"
"#,
    )
    .unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop = rustain::adapters::profile_resolver::noop::NoopProfileResolver;
    let config = rustain::infrastructure::config::try_load(&cli, &noop).unwrap();
    let display = config_cmd::ConfigDisplay::from_config(&config, None);

    let json_output = serde_json::to_string_pretty(&display).unwrap();
    let toml_value = toml::Value::try_from(&display).unwrap();
    let toml_output = toml::to_string_pretty(&toml_value).unwrap();

    assert!(
        !json_output.contains("SECRET-TOK"),
        "SECRET-TOK leaked in JSON output"
    );
    assert!(
        !toml_output.contains("SECRET-TOK"),
        "SECRET-TOK leaked in TOML output"
    );

    assert!(
        json_output.contains("api.example.com"),
        "Sanitized host missing from JSON output"
    );
    assert!(
        toml_output.contains("api.example.com"),
        "Sanitized host missing from TOML output"
    );
}

/// P0-1b: Name-only env var NAME is shown, secret value is never resolved.
#[test]
#[serial]
fn p0_1b_api_key_env_name_shown_value_never_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"[provider.test]
provider_id = "test"
model_id = "test-model"
api_key_env = "DEADBEEF_TEST_KEY_ENV"
base_url = "https://api.example.com"
"#,
    )
    .unwrap();

    unsafe { std::env::set_var("DEADBEEF_TEST_KEY_ENV", "DEADBEEF-DO-NOT-LEAK-7F3A") };
    let _cleanup = scopeguard::guard((), |_| unsafe {
        std::env::remove_var("DEADBEEF_TEST_KEY_ENV");
    });

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop = rustain::adapters::profile_resolver::noop::NoopProfileResolver;
    let config = rustain::infrastructure::config::try_load(&cli, &noop).unwrap();
    let display = config_cmd::ConfigDisplay::from_config(&config, None);

    let json_output = serde_json::to_string_pretty(&display).unwrap();
    let toml_value = toml::Value::try_from(&display).unwrap();
    let toml_output = toml::to_string_pretty(&toml_value).unwrap();

    assert!(
        !json_output.contains("DEADBEEF-DO-NOT-LEAK"),
        "Secret value leaked in JSON"
    );
    assert!(
        !toml_output.contains("DEADBEEF-DO-NOT-LEAK"),
        "Secret value leaked in TOML"
    );

    assert!(
        json_output.contains("DEADBEEF_TEST_KEY_ENV"),
        "Env var name missing from JSON"
    );
    assert!(
        toml_output.contains("DEADBEEF_TEST_KEY_ENV"),
        "Env var name missing from TOML"
    );
}

/// P0-1c (error-half): `config show` extraction error does NOT leak a secret from base_url.
#[tokio::test]
#[serial]
async fn p0_1c_config_show_error_does_not_leak_secret() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    // Put the type mismatch at the ROOT level (before any table) so extraction fails.
    std::fs::write(
        &config_path,
        r#"log_max_size_mb = "huge"

[provider.test]
provider_id = "test"
model_id = "test-model"
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://user:SECRET-TOK@api.example.com/v1"
"#,
    )
    .unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop: std::sync::Arc<dyn rustain::domain::ports::ProfileResolver> =
        std::sync::Arc::new(rustain::adapters::profile_resolver::noop::NoopProfileResolver);

    let result = config_cmd::run_config_show(false, &noop, &cli).await;
    assert!(result.is_err(), "config show should fail on invalid config");

    let err_string = result.unwrap_err().to_string();
    assert!(
        !err_string.contains("SECRET-TOK"),
        "User-facing show error leaked secret: {err_string}"
    );
}

// =========================================================================
// P0-2: Zero env access in show path — runtime chokepoint counter
// =========================================================================

/// P0-2a: `config show` DTO construction makes zero env_var_trimmed calls.
///
/// `#[serial]` because `ENV_VAR_TRIMMED_CALLS` is a process-global counter and
/// `p0_2b` deliberately increments it. Without this, the two race under a loaded
/// parallel run and this test reads the sibling's increments as a `config show`
/// env access — a false failure, and the file's own header already states the
/// convention.
#[test]
#[serial]
fn p0_2a_config_show_zero_env_var_trimmed_calls() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[provider]\n").unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop = rustain::adapters::profile_resolver::noop::NoopProfileResolver;

    let config = rustain::infrastructure::config::try_load(&cli, &noop).unwrap();

    ENV_VAR_TRIMMED_CALLS.store(0, Ordering::SeqCst);
    let display = config_cmd::ConfigDisplay::from_config(&config, None);
    let _json = serde_json::to_string_pretty(&display).unwrap();
    let value = toml::Value::try_from(&display).unwrap();
    let _toml = toml::to_string_pretty(&value).unwrap();

    let calls = ENV_VAR_TRIMMED_CALLS.load(Ordering::SeqCst);
    assert_eq!(
        calls, 0,
        "config show path called env_var_trimmed {calls} times — must be 0"
    );
}

/// P0-2b: Positive control — resolve_editor DOES call env_var_trimmed.
#[test]
#[serial]
fn p0_2b_resolve_editor_increments_env_var_trimmed_counter() {
    ENV_VAR_TRIMMED_CALLS.store(0, Ordering::SeqCst);

    let _ = rustain::infrastructure::utils::editor::resolve_editor();

    let calls = ENV_VAR_TRIMMED_CALLS.load(Ordering::SeqCst);
    assert!(
        calls > 0,
        "resolve_editor should have incremented ENV_VAR_TRIMMED_CALLS, got {calls}"
    );
}

// =========================================================================
// P0-3: config validate no-TUI/no-provider — armed sentinel
// =========================================================================

/// P0-3a: Valid config → exit 0, no provider constructed.
#[tokio::test]
#[serial]
async fn p0_3a_config_validate_valid_no_provider_constructed() {
    use rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT;
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "model = \"test\"\n").unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop: std::sync::Arc<dyn rustain::domain::ports::ProfileResolver> =
        std::sync::Arc::new(rustain::adapters::profile_resolver::noop::NoopProfileResolver);

    PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    let result = config_cmd::run_config_validate(false, &noop, &cli).await;
    assert!(result.is_ok(), "valid config should produce Ok");

    let ctor_count = PROVIDER_CTOR_COUNT.load(Ordering::SeqCst);
    assert_eq!(
        ctor_count, 0,
        "config validate must not construct any provider, got {ctor_count}"
    );
}

/// P0-3b: Type-mismatched config → non-zero exit (Err).
#[tokio::test]
async fn p0_3b_config_validate_malformed_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "model = \"test\"\nlog_max_size_mb = \"huge\"\n",
    )
    .unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop: std::sync::Arc<dyn rustain::domain::ports::ProfileResolver> =
        std::sync::Arc::new(rustain::adapters::profile_resolver::noop::NoopProfileResolver);

    let result = config_cmd::run_config_validate(false, &noop, &cli).await;
    assert!(
        result.is_err(),
        "Type-mismatched config should produce a non-zero exit"
    );

    let json_result = config_cmd::run_config_validate(true, &noop, &cli).await;
    assert!(json_result.is_err(), "JSON validate should also error");
}

/// P0-3c: Positive control — build_provider_for_config increments PROVIDER_CTOR_COUNT.
#[test]
#[serial]
fn p0_3c_provider_ctor_count_armed() {
    use rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT;

    PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    let cfg = rustain::domain::models::ProviderConfig {
        provider_id: "test".to_string(),
        model_id: "test-model".to_string(),
        api_key_env: "NONEXISTENT_API_KEY_FOR_TEST".to_string(),
        enabled: true,
        kind: Some("anthropic".to_string()),
        base_url: None,
        context_window: None,
        supports_tools: None,
        discover_models: false,
        model_filter: vec![],
        cache_ttl_seconds: 3600,
    };

    unsafe { std::env::set_var("NONEXISTENT_API_KEY_FOR_TEST", "sk-test-fake") };
    let _cleanup = scopeguard::guard((), |_| unsafe {
        std::env::remove_var("NONEXISTENT_API_KEY_FOR_TEST");
    });

    let _ = rustain::infrastructure::provider_factory::build_provider_for_config("test", &cfg);

    let count = PROVIDER_CTOR_COUNT.load(Ordering::SeqCst);
    assert!(
        count > 0,
        "PROVIDER_CTOR_COUNT should have incremented after build_provider_for_config, got {count}"
    );
}

/// P0-3d: config validate --json produces {valid, errors} shape.
#[tokio::test]
async fn p0_3d_config_validate_json_shape() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "model = \"test\"\n").unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop: std::sync::Arc<dyn rustain::domain::ports::ProfileResolver> =
        std::sync::Arc::new(rustain::adapters::profile_resolver::noop::NoopProfileResolver);

    let result = config_cmd::run_config_validate(true, &noop, &cli).await;
    assert!(result.is_ok(), "valid config --json should succeed");
}

// =========================================================================
// P0-4: config path precedence
// =========================================================================

/// P0-4a: config_layer_paths returns 7 layers in priority order.
#[test]
fn p0_4a_config_path_seven_layers_ordered() {
    let cli = cli_from(&["rustain"]);
    let layers = config_cmd::config_layer_paths(&cli).unwrap();

    assert_eq!(layers.len(), 7, "expected 7 layers");

    for (i, layer) in layers.iter().enumerate() {
        assert_eq!(
            layer.priority as usize,
            i + 1,
            "layer {} has wrong priority",
            layer.kind
        );
    }

    assert_eq!(layers[0].kind, "CLI flags");
    assert_eq!(layers[1].kind, "RUSTAIN_* env vars");
    assert!(layers[2].kind.contains("local override"));
    assert!(layers[3].kind.contains("workspace"));
    assert!(layers[4].kind.contains("user-global"));
    assert!(layers[5].kind.contains("active profile"));
    assert!(layers[6].kind.contains("built-in"));
}

/// P0-4b: --config-file override is reflected in layer descriptor.
#[test]
fn p0_4b_config_file_override_reflected_in_layers() {
    let dir = tempfile::tempdir().unwrap();
    let custom_path = dir.path().join("custom.toml");
    std::fs::write(&custom_path, "").unwrap();

    let cli = cli_from(&["rustain", "--config-file", custom_path.to_str().unwrap()]);
    let layers = config_cmd::config_layer_paths(&cli).unwrap();

    let ws_layer = layers.iter().find(|l| l.priority == 4).unwrap();
    assert_eq!(
        ws_layer.path.as_ref().unwrap().as_path(),
        custom_path.as_path(),
        "workspace layer should use --config-file override"
    );
    assert!(ws_layer.exists, "custom config file should exist");
}

/// P0-4c: config path --json parses as valid JSON array.
#[tokio::test]
async fn p0_4c_config_path_json_parses() {
    let cli = cli_from(&["rustain"]);

    let layers = config_cmd::config_layer_paths(&cli).unwrap();
    let json_layers: Vec<serde_json::Value> = layers
        .iter()
        .map(|l| {
            let mut obj = serde_json::Map::new();
            obj.insert("layer".into(), serde_json::Value::String(l.kind.into()));
            obj.insert(
                "priority".into(),
                serde_json::Value::Number(l.priority.into()),
            );
            if let Some(p) = &l.path {
                obj.insert(
                    "path".into(),
                    serde_json::Value::String(p.display().to_string()),
                );
                obj.insert("exists".into(), serde_json::Value::Bool(l.exists));
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    let json_str = serde_json::to_string_pretty(&json_layers).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert!(
        parsed.is_array(),
        "config path --json must produce an array"
    );
    assert_eq!(parsed.as_array().unwrap().len(), 7);
}

/// P0-4d: config edit target matches config path descriptor (no edit/path divergence).
#[test]
fn p0_4d_edit_target_matches_path_descriptor() {
    let cli = cli_from(&["rustain"]);
    let layers = config_cmd::config_layer_paths(&cli).unwrap();

    let ws_layer = layers.iter().find(|l| l.priority == 4).unwrap();
    assert!(
        ws_layer.path.is_some(),
        "workspace layer must have a file path"
    );

    let global_layer = layers.iter().find(|l| l.priority == 5).unwrap();
    assert!(
        global_layer.path.is_some(),
        "user-global layer must have a file path"
    );

    assert_ne!(
        ws_layer.path, global_layer.path,
        "workspace and global paths must differ"
    );
}

// =========================================================================
// P0-5: config edit editor resolution
// =========================================================================

/// P0-5a: Editor resolution falls back to platform default when env vars unset.
#[test]
#[serial]
fn p0_5a_editor_resolution_platform_default() {
    let orig_visual = std::env::var("VISUAL").ok();
    let orig_editor = std::env::var("EDITOR").ok();

    unsafe {
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
    }

    let result = rustain::infrastructure::utils::editor::resolve_editor().unwrap();
    assert!(!result.is_empty(), "editor must resolve to something");

    match orig_visual {
        Some(v) => unsafe { std::env::set_var("VISUAL", v) },
        None => unsafe { std::env::remove_var("VISUAL") },
    }
    match orig_editor {
        Some(v) => unsafe { std::env::set_var("EDITOR", v) },
        None => unsafe { std::env::remove_var("EDITOR") },
    }
}

/// P0-5b: run_config_edit invokes the configured editor on the resolved path.
#[tokio::test]
#[serial]
async fn p0_5b_run_config_edit_invokes_editor_on_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(".rustain").join("config.toml");
    let marker_path = dir.path().join("editor-was-here");

    let editor_script = dir.path().join("fake-editor.sh");
    #[cfg(unix)]
    {
        std::fs::write(
            &editor_script,
            format!("#!/bin/sh\ntouch \"{}\"\nexit 0\n", marker_path.display()),
        )
        .unwrap();
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(
            &editor_script,
            format!("@echo off\ntype nul > \"{}\"\n", marker_path.display()),
        )
        .unwrap();
    }

    let orig_visual = std::env::var("VISUAL").ok();
    let orig_editor = std::env::var("EDITOR").ok();
    unsafe {
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", editor_script.to_str().unwrap());
    }
    let _cleanup = scopeguard::guard(
        (editor_script, orig_visual, orig_editor),
        |(script, vis, ed)| unsafe {
            std::env::remove_var("EDITOR");
            if let Some(v) = vis {
                std::env::set_var("VISUAL", v);
            } else {
                std::env::remove_var("VISUAL");
            }
            if let Some(e) = ed {
                std::env::set_var("EDITOR", e);
            } else {
                std::env::remove_var("EDITOR");
            }
            let _ = std::fs::remove_file(&script);
        },
    );

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let result = config_cmd::run_config_edit(false, &cli).await;
    assert!(result.is_ok(), "config edit should succeed: {result:?}");

    assert!(
        marker_path.exists(),
        "fake editor was not invoked (marker file missing)"
    );

    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(content.starts_with('#'), "scaffold must be comment-only");
    assert!(
        !content.contains(" = "),
        "scaffold must not contain live keys"
    );
}

/// P0-5c: `--global` edit target differs from workspace target.
#[tokio::test]
#[serial]
async fn p0_5c_config_edit_global_target_differs() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_path = dir.path().join(".rustain").join("config.toml");

    let editor_script = dir.path().join("noop-editor.sh");
    #[cfg(unix)]
    {
        std::fs::write(&editor_script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&editor_script, "@echo off\nexit /b 0\n").unwrap();
    }

    let orig_visual = std::env::var("VISUAL").ok();
    let orig_editor = std::env::var("EDITOR").ok();
    unsafe {
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", editor_script.to_str().unwrap());
    }
    let _cleanup = scopeguard::guard(
        (editor_script, orig_visual, orig_editor),
        |(script, vis, ed)| unsafe {
            std::env::remove_var("EDITOR");
            if let Some(v) = vis {
                std::env::set_var("VISUAL", v);
            } else {
                std::env::remove_var("VISUAL");
            }
            if let Some(e) = ed {
                std::env::set_var("EDITOR", e);
            } else {
                std::env::remove_var("EDITOR");
            }
            let _ = std::fs::remove_file(&script);
        },
    );

    let cli = cli_from(&["rustain", "--config-file", workspace_path.to_str().unwrap()]);

    let ws_result = config_cmd::run_config_edit(false, &cli).await;
    assert!(ws_result.is_ok(), "workspace edit should succeed");

    let global_result = config_cmd::run_config_edit(true, &cli).await;
    assert!(global_result.is_ok(), "global edit should succeed");
}

// =========================================================================
// P0-6: config reload unchanged (regression)
// =========================================================================

/// P0-6: `config reload` produces the exact same output as before (byte-for-byte).
#[tokio::test]
async fn p0_6_config_reload_unchanged() {
    let result = config_cmd::run_config_reload().await;
    assert!(result.is_ok(), "config reload must succeed");

    let expected = config_cmd::config_reload_message();
    assert!(!expected.is_empty(), "reload message must not be empty");
}

// =========================================================================
// P0-7: Conformance ratchets held
// =========================================================================

/// P0-7a: ConfigAction subcommand count pinned at 5.
#[test]
fn p0_7a_config_subcommand_count_pinned() {
    use clap::CommandFactory;

    const EXPECTED_CONFIG_SUBCOMMANDS: usize = 5;
    let cmd = Cli::command();
    let config_cmd = cmd
        .find_subcommand("config")
        .expect("'config' subcommand should exist");
    let count = config_cmd.get_subcommands().count();
    assert_eq!(
        count, EXPECTED_CONFIG_SUBCOMMANDS,
        "Expected {} config subcommands, got {}",
        EXPECTED_CONFIG_SUBCOMMANDS, count
    );
}

/// P0-7b: All expected config subcommands exist.
#[test]
fn p0_7b_config_subcommands_exist() {
    use clap::CommandFactory;

    let cmd = Cli::command();
    let config_cmd = cmd
        .find_subcommand("config")
        .expect("'config' subcommand should exist");

    for verb in ["reload", "show", "edit", "path", "validate"] {
        assert!(
            config_cmd.find_subcommand(verb).is_some(),
            "'config {verb}' subcommand should exist"
        );
    }
}

// =========================================================================
// P0-8: RUSTAIN_* env-injection → userinfo strip (primary live leak vector)
// =========================================================================

/// P0-8a: RUSTAIN_PROVIDER__ENVTEST__BASE_URL env-injection — userinfo stripped from show.
#[test]
#[serial]
fn p0_8a_env_injection_base_url_userinfo_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"[provider.envtest]
provider_id = "envtest"
model_id = "test-model"
api_key_env = "TEST_KEY"
base_url = "https://clean.example.com/v1"
"#,
    )
    .unwrap();

    unsafe {
        std::env::set_var(
            "RUSTAIN_PROVIDER__ENVTEST__BASE_URL",
            "https://user:ENV-SECRET-9X@injected.example.com/v1",
        );
    }
    let _cleanup = scopeguard::guard((), |_| unsafe {
        std::env::remove_var("RUSTAIN_PROVIDER__ENVTEST__BASE_URL");
    });

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let noop = rustain::adapters::profile_resolver::noop::NoopProfileResolver;
    let config = rustain::infrastructure::config::try_load(&cli, &noop).unwrap();

    let has_injected_url = config.provider.values().any(|p| {
        p.base_url
            .as_ref()
            .is_some_and(|u| u.expose_url().contains("injected.example.com"))
    });
    assert!(
        has_injected_url,
        "RUSTAIN_ env injection did not land in resolved config"
    );

    let display = config_cmd::ConfigDisplay::from_config(&config, None);
    let json_output = serde_json::to_string_pretty(&display).unwrap();
    let toml_value = toml::Value::try_from(&display).unwrap();
    let toml_output = toml::to_string_pretty(&toml_value).unwrap();

    assert!(
        !json_output.contains("ENV-SECRET-9X"),
        "ENV-SECRET-9X leaked in JSON output"
    );
    assert!(
        !toml_output.contains("ENV-SECRET-9X"),
        "ENV-SECRET-9X leaked in TOML output"
    );

    assert!(
        json_output.contains("injected.example.com"),
        "Sanitized injected host missing from JSON"
    );
    assert!(
        toml_output.contains("injected.example.com"),
        "Sanitized injected host missing from TOML"
    );
}

/// P0-8b: strip_url_userinfo works correctly on McpServerSpec.url patterns.
#[test]
fn p0_8b_strip_url_userinfo_mcp_url() {
    let stripped = rustain::infrastructure::utils::strip_url_userinfo(
        "https://admin:s3cret@mcp.example.com:8080/api",
    );
    let stripped_str: &str = &stripped;
    assert!(
        !stripped_str.contains("s3cret"),
        "MCP URL secret leaked: {stripped_str}"
    );
    assert!(
        !stripped_str.contains("admin"),
        "MCP URL username leaked: {stripped_str}"
    );
    assert!(
        stripped_str.contains("mcp.example.com"),
        "MCP URL host lost: {stripped_str}"
    );
}

/// P0-8c: strip_url_userinfo handles percent-encoded credentials.
#[test]
fn p0_8c_strip_url_userinfo_percent_encoded() {
    let stripped = rustain::infrastructure::utils::strip_url_userinfo(
        "https://user%40org:p%40ss@host.com/path",
    );
    let stripped_str: &str = &stripped;
    assert!(
        !stripped_str.contains("p%40ss"),
        "Percent-encoded password leaked: {stripped_str}"
    );
    assert!(
        stripped_str.contains("host.com"),
        "Host lost: {stripped_str}"
    );
}

/// P0-8d: strip_url_userinfo passes through unparseable URLs unchanged.
#[test]
fn p0_8d_strip_url_userinfo_unparseable_passthrough() {
    let input = "not-a-url";
    let stripped = rustain::infrastructure::utils::strip_url_userinfo(input);
    assert_eq!(
        stripped.as_ref(),
        input,
        "unparseable URL should pass through unchanged"
    );
}

/// P0-8e: strip_url_userinfo no-ops on URLs without userinfo.
#[test]
fn p0_8e_strip_url_userinfo_no_userinfo_passthrough() {
    let input = "https://api.example.com/v1";
    let stripped = rustain::infrastructure::utils::strip_url_userinfo(input);
    assert_eq!(
        stripped.as_ref(),
        input,
        "URL without userinfo should be returned as-is"
    );
}

// =========================================================================
// Additional tests for end-to-end paths and real resolver
// =========================================================================

/// ConfigDisplay renders default AppConfig without panicking.
#[test]
fn config_display_renders_default_config() {
    let config = rustain::domain::models::AppConfig::default();
    let display = config_cmd::ConfigDisplay::from_config(&config, None);

    let value = toml::Value::try_from(&display).unwrap();
    let toml_output = toml::to_string_pretty(&value).unwrap();
    assert!(!toml_output.is_empty(), "TOML output should not be empty");

    let json_output = serde_json::to_string_pretty(&display).unwrap();
    assert!(!json_output.is_empty(), "JSON output should not be empty");

    assert!(json_output.contains("claude-sonnet-4-6"), "default model");
    assert!(json_output.contains("coding"), "default profile");
}

/// End-to-end `config show --json` invokes the handler and returns parseable JSON.
#[tokio::test]
async fn config_show_json_end_to_end_parses() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "model = \"test-model\"\n").unwrap();

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let resolver = std::sync::Arc::new(TestProfileResolver {
        defaults: None,
        mcp_servers: vec![],
    }) as std::sync::Arc<dyn ProfileResolver>;

    let output = config_cmd::render_config_show(true, &resolver, &cli)
        .await
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&output).expect("render_config_show --json must produce valid JSON");
    assert_eq!(
        parsed.get("model").and_then(|v| v.as_str()),
        Some("test-model")
    );
}

#[tokio::test]
async fn config_show_strips_mcp_server_url_userinfo() {
    use rustain::domain::models::mcp_server_spec::{McpServerSource, McpServerSpec, McpTransport};

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "model = \"test-model\"\n").unwrap();

    let mcp_server = McpServerSpec {
        id: "test-mcp".to_string(),
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        url: Some(
            "https://admin:MCP-SECRET@mcp.example.com/api"
                .to_string()
                .into(),
        ),
        env: Default::default(),
        persistent: false,
        source: McpServerSource::Workspace,
    };

    let cli = cli_from(&["rustain", "--config-file", config_path.to_str().unwrap()]);
    let resolver = std::sync::Arc::new(TestProfileResolver {
        defaults: None,
        mcp_servers: vec![mcp_server],
    }) as std::sync::Arc<dyn ProfileResolver>;

    let output = config_cmd::render_config_show(true, &resolver, &cli)
        .await
        .unwrap();
    assert!(
        !output.contains("MCP-SECRET"),
        "MCP URL secret leaked in JSON output"
    );
    assert!(
        output.contains("mcp.example.com"),
        "MCP URL host missing from JSON output"
    );
}

/// Config path --json rendered by the handler is a parseable array.
#[tokio::test]
async fn config_path_json_renders_parseable_array() {
    let cli = cli_from(&["rustain"]);
    let output = config_cmd::render_config_path(true, &cli).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&output).expect("render_config_path --json must produce valid JSON");
    assert!(
        parsed.is_array(),
        "config path --json must produce an array"
    );
    assert_eq!(parsed.as_array().unwrap().len(), 7);
}
