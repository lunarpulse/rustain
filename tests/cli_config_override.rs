//! Integration tests for `-c key=value` CLI config overrides (Story 13.6).
//!
//! Every command path here is offline-only; provider construction is guarded by
//! PROVIDER_CTOR_COUNT in the test-instrumentation build.

#![cfg(feature = "test-instrumentation")]

use std::sync::atomic::Ordering;

use assert_cmd::Command;
use predicates::prelude::*;
use serial_test::serial;

fn assert_no_provider_constructed(context: &str) {
    let count =
        rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.load(Ordering::SeqCst);
    assert_eq!(
        count, 0,
        "{context} must not construct providers; got {count}"
    );
}

#[test]
fn parse_config_overrides_accepts_dot_paths_and_scalars() {
    let pairs = vec![
        "provider.ollama.base_url=http://localhost:11434/v1".to_string(),
        "provider.ollama.model_id=llama3.2".to_string(),
        "router.threshold_tokens=100000".to_string(),
        "default_plan_mode=true".to_string(),
        "model=".to_string(),
    ];

    let value = rustain::infrastructure::config::parse_config_overrides(&pairs)
        .expect("valid overrides parse");

    assert_eq!(
        value
            .pointer("/provider/ollama/base_url")
            .and_then(|v| v.as_str()),
        Some("http://localhost:11434/v1")
    );
    assert_eq!(
        value
            .pointer("/provider/ollama/model_id")
            .and_then(|v| v.as_str()),
        Some("llama3.2"),
        "sibling provider overrides must deep-merge, not replace"
    );
    assert_eq!(
        value
            .pointer("/router/threshold_tokens")
            .and_then(|v| v.as_u64()),
        Some(100000),
        "numeric leaf values must be JSON numbers, not strings"
    );
    assert_eq!(
        value
            .pointer("/default_plan_mode")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(value.pointer("/model").and_then(|v| v.as_str()), Some(""));
}

#[test]
fn parse_config_overrides_rejects_credentials_but_not_substrings() {
    for key in [
        "api_key_env=X",
        "provider.openai.api_key_env=X",
        "auth.token=X",
        "secret=X",
    ] {
        let err = rustain::infrastructure::config::parse_config_overrides(&[key.to_string()])
            .expect_err("credential key must be rejected")
            .to_string();
        assert!(
            err.contains("credential"),
            "error must mention credential: {err}"
        );
        assert!(
            err.contains("rustain auth login"),
            "error must point to auth login: {err}"
        );
    }

    for key in [
        "tokenizer=cl100k",
        "secretary=enabled",
        "credentials_note=metadata-only",
    ] {
        rustain::infrastructure::config::parse_config_overrides(&[key.to_string()])
            .unwrap_or_else(|e| panic!("substring-only key must be accepted: {key}: {e}"));
    }
}

#[test]
fn parse_config_overrides_rejects_malformed_paths() {
    for key in ["model", "=value", "..key=value", "key..sub=value"] {
        let err = rustain::infrastructure::config::parse_config_overrides(&[key.to_string()])
            .expect_err("malformed override must fail")
            .to_string();
        assert!(
            err.contains("KEY=VALUE") || err.contains("dot-path"),
            "{err}"
        );
    }
}

#[test]
fn known_field_names_match_app_config_serde_fields() {
    let default = serde_json::to_value(rustain::domain::models::AppConfig::default())
        .expect("AppConfig default serializes");
    let object = default.as_object().expect("AppConfig serializes to object");
    let mut actual: Vec<&str> = object.keys().map(String::as_str).collect();
    actual.sort_unstable();

    let mut expected = rustain::infrastructure::config::KNOWN_TOP_LEVEL_FIELDS.to_vec();
    expected.sort_unstable();

    assert_eq!(
        actual, expected,
        "KNOWN_TOP_LEVEL_FIELDS drifted from AppConfig"
    );
}

#[test]
#[serial]
fn config_show_c_override_beats_typed_model_flag_without_provider() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .args([
            "--model",
            "from-typed-flag",
            "-c",
            "model=from-c-flag",
            "config",
            "show",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("from-c-flag"))
        .stdout(predicate::str::contains("from-typed-flag").not());

    assert_no_provider_constructed("config show -c precedence");
}

#[test]
#[serial]
fn config_path_reports_c_override_layer_without_provider() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .args(["config", "path", "-c", "model=X"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0. · -c CLI overrides"));

    assert_no_provider_constructed("config path -c layer");
}

#[test]
#[serial]
fn config_show_rejects_credential_override_before_loading_config() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .args(["config", "show", "-c", "provider.ollama.api_key_env=LEAKED"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("credential"))
        .stderr(predicate::str::contains("rustain auth login"));

    assert_no_provider_constructed("credential rejection");
}

#[test]
fn config_override_rejects_conflicting_nested_paths() {
    let err = rustain::infrastructure::config::parse_config_overrides(&[
        "model=gpt-4o".to_string(),
        "model.name=o3".to_string(),
    ])
    .expect_err("conflicting nested keys must fail")
    .to_string();
    assert!(
        err.contains("Conflicting -c overrides"),
        "error must describe conflict: {err}"
    );
}

#[test]
fn config_override_later_value_overwrites_earlier() {
    let value = rustain::infrastructure::config::parse_config_overrides(&[
        "model=first".to_string(),
        "model=second".to_string(),
    ])
    .expect("same-key overrides parse");
    assert_eq!(
        value.pointer("/model").and_then(|v| v.as_str()),
        Some("second")
    );
}

#[test]
fn config_override_rejects_non_scalar_values() {
    for key in ["model=[1,2]", "model={\"a\":1}", "model=null"] {
        let err = rustain::infrastructure::config::parse_config_overrides(&[key.to_string()])
            .expect_err("non-scalar value must fail")
            .to_string();
        assert!(
            err.contains("arrays, objects, and null are not supported"),
            "{err}"
        );
    }
}

#[test]
#[serial]
fn config_show_c_override_beats_env_var_without_provider() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_MODEL", "from-env")
        .args(["-c", "model=from-c-flag", "config", "show", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from-c-flag"))
        .stdout(predicate::str::contains("from-env").not());

    assert_no_provider_constructed("config show -c precedence over env");
}

#[test]
#[serial]
fn config_path_reports_eight_layers_and_singular_pair_count() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .args(["config", "path", "-c", "model=X"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0. · -c CLI overrides (1 pair)"));

    assert_no_provider_constructed("config path -c layer count");
}

#[test]
#[serial]
fn config_path_reports_plural_pair_count() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .args(["config", "path", "-c", "model=X", "-c", "log_level=debug"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0. · -c CLI overrides (2 pairs)"));

    assert_no_provider_constructed("config path -c plural pairs");
}

#[test]
#[serial]
fn config_show_warns_unknown_top_level_key_to_stderr() {
    rustain::infrastructure::provider_factory::PROVIDER_CTOR_COUNT.store(0, Ordering::SeqCst);

    Command::cargo_bin("rustain")
        .unwrap()
        .args(["-c", "nonexistent.key=value", "config", "show", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("unknown config key 'nonexistent'"))
        .stderr(predicate::str::contains(
            "The key will still be passed to the config loader",
        ));

    assert_no_provider_constructed("unknown key warning");
}
