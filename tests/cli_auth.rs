//! Integration tests for `rustain auth login` (Story 13.4a).
//!
//! Covers P0-4 (env precedence ± negative control), P0-7 (unknown provider),
//! P0-8 (keyless provider).

use serial_test::serial;

// ---------------------------------------------------------------------------
// P0-4: Env-precedence positive AND negative control (AC7)
// ---------------------------------------------------------------------------

/// P0-4 negative control: auth.json has a key, NO env var → resolution yields auth.json key.
#[test]
#[serial]
fn resolve_api_key_auth_json_fallback() {
    use rustain::infrastructure::provider_factory;

    let tmp = tempfile::TempDir::new().unwrap();
    // SAFETY: single-threaded via #[serial].
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", tmp.path());
        // Ensure the env var for this test provider is NOT set
        std::env::remove_var("TEST_AUTH_PROVIDER_KEY");
    }

    // Write a test auth.json manually
    let auth_json = serde_json::json!({
        "version": 1,
        "providers": {
            "test-provider": {
                "type": "api_key",
                "api_key": "stored-in-auth-json",
                "last_validated": "2026-06-16T00:00:00Z"
            }
        }
    });
    std::fs::write(
        tmp.path().join("auth.json"),
        serde_json::to_string_pretty(&auth_json).unwrap(),
    )
    .unwrap();

    let key = provider_factory::resolve_api_key("TEST_AUTH_PROVIDER_KEY", "test-provider");
    assert_eq!(
        key.as_deref(),
        Some("stored-in-auth-json"),
        "Negative control: auth.json key should be used when no env var"
    );

    // Cleanup
    unsafe {
        std::env::remove_var("RUSTAIN_DATA_DIR");
    }
}

/// P0-4 positive control: env var set → resolution yields env var (auth.json ignored).
#[test]
#[serial]
fn resolve_api_key_env_var_wins_over_auth_json() {
    use rustain::infrastructure::provider_factory;

    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", tmp.path());
        std::env::set_var("TEST_AUTH_ENV_WIN_KEY", "from-env-var");
    }

    // Write auth.json with a different key
    let auth_json = serde_json::json!({
        "version": 1,
        "providers": {
            "test-provider": {
                "type": "api_key",
                "api_key": "from-auth-json",
                "last_validated": "2026-06-16T00:00:00Z"
            }
        }
    });
    std::fs::write(
        tmp.path().join("auth.json"),
        serde_json::to_string_pretty(&auth_json).unwrap(),
    )
    .unwrap();

    let key = provider_factory::resolve_api_key("TEST_AUTH_ENV_WIN_KEY", "test-provider");
    assert_eq!(
        key.as_deref(),
        Some("from-env-var"),
        "Positive control: env var MUST win over auth.json"
    );

    // Cleanup
    unsafe {
        std::env::remove_var("TEST_AUTH_ENV_WIN_KEY");
        std::env::remove_var("RUSTAIN_DATA_DIR");
    }
}

/// P0-4 typed: resolve_auth returns ResolvedAuth::ApiKey
#[test]
#[serial]
fn resolve_auth_returns_typed_api_key() {
    use rustain::domain::models::credential::ResolvedAuth;
    use rustain::infrastructure::provider_factory;

    let tmp = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", tmp.path());
        std::env::set_var("TEST_RESOLVE_AUTH_KEY", "typed-key");
    }

    let auth =
        provider_factory::resolve_auth("TEST_RESOLVE_AUTH_KEY", "any").expect("should resolve");
    match auth {
        ResolvedAuth::ApiKey(k) => {
            assert_eq!(k, "typed-key");
        }
        _ => panic!("Expected ResolvedAuth::ApiKey"),
    }

    unsafe {
        std::env::remove_var("TEST_RESOLVE_AUTH_KEY");
        std::env::remove_var("RUSTAIN_DATA_DIR");
    }
}

// ---------------------------------------------------------------------------
// P0-7: Unknown provider
// ---------------------------------------------------------------------------

/// P0-7: `rustain auth login nonexistent-provider` should fail with valid provider list.
#[test]
fn auth_login_unknown_provider_fails() {
    use assert_cmd::Command;
    use predicates::prelude::*;

    Command::cargo_bin("rustain")
        .unwrap()
        .args(["auth", "login", "nonexistent-provider"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("unknown provider")
                .or(predicate::str::contains("Valid providers")),
        );
}

// ---------------------------------------------------------------------------
// P0-8: Keyless provider (ollama)
// ---------------------------------------------------------------------------

/// P0-8: Keyless provider (ollama) exits 0, nothing written.
#[test]
fn auth_login_ollama_reports_no_key_required() {
    use assert_cmd::Command;
    use predicates::prelude::*;

    let tmp = tempfile::TempDir::new().unwrap();
    Command::cargo_bin("rustain")
        .unwrap()
        .args(["auth", "login", "ollama"])
        .env("RUSTAIN_DATA_DIR", tmp.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("does not require").or(predicate::str::contains("no API key")),
        );

    // Verify no auth.json was created
    assert!(
        !tmp.path().join("auth.json").exists(),
        "No auth.json should be created for keyless provider"
    );
}

/// P0-8b: Keyless provider with --json
#[test]
fn auth_login_ollama_json_output() {
    use assert_cmd::Command;
    use predicates::prelude::*;

    let tmp = tempfile::TempDir::new().unwrap();
    Command::cargo_bin("rustain")
        .unwrap()
        .args(["auth", "login", "ollama", "--json"])
        .env("RUSTAIN_DATA_DIR", tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("keyless"));
}
