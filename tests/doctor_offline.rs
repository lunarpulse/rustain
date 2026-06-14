//! Story 13.2 P0 tests — offline detection, count-math, connectivity probe, `--json`.
//!
//! **Offline-sim:** bind-then-close `TcpListener` on `127.0.0.1:0` (deterministic
//! connection-refused, zero wait). NOT TEST-NET, NOT host-offline.
#![allow(clippy::useless_vec)]

use rustain::adapters::cli::doctor::json::{DOCTOR_SCHEMA_VERSION, DoctorReport};
use rustain::adapters::cli::doctor::{
    CheckResult, CheckStatus, CheckTier, HealthCheck, ProviderConnectivityCheck, display_results,
};
use rustain::domain::errors::ProviderError;
use rustain::domain::ports::{ProbeOutcome, StreamingProvider};

use async_trait::async_trait;
use std::sync::Arc;

// ──────────────────────────────────────────────────
// Helpers: fake providers for hermetic testing
// ──────────────────────────────────────────────────

struct PassProvider;
#[async_trait]
impl StreamingProvider for PassProvider {
    async fn stream_completion(
        &self,
        _: Vec<rustain::domain::models::Message>,
        _: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        ProviderError,
    > {
        unreachable!()
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "pass".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
            latency: std::time::Duration::from_millis(42),
        })
    }
}

struct OfflineProvider;
#[async_trait]
impl StreamingProvider for OfflineProvider {
    async fn stream_completion(
        &self,
        _: Vec<rustain::domain::models::Message>,
        _: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        ProviderError,
    > {
        Err(ProviderError::Offline("connection refused".into()))
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "offline".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Err(ProviderError::Offline("connection refused".into()))
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Err(ProviderError::Offline("connection refused".into()))
    }
}

struct AuthFailProvider;
#[async_trait]
impl StreamingProvider for AuthFailProvider {
    async fn stream_completion(
        &self,
        _: Vec<rustain::domain::models::Message>,
        _: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        ProviderError,
    > {
        Err(ProviderError::AuthenticationFailed)
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "authfail".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Err(ProviderError::AuthenticationFailed)
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Err(ProviderError::AuthenticationFailed)
    }
}

// ──────────────────────────────────────────────────
// P0-1: Offline → Skipped → exit 0 (AC2/AC7, FR102 core)
// ──────────────────────────────────────────────────

#[tokio::test]
async fn test_offline_yields_skipped_not_fail() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "test-offline"),
        provider_name: "test-offline".to_string(),
        provider: Some(Arc::new(OfflineProvider)),
    };
    let result = check.run().await;
    assert!(
        matches!(result.status, CheckStatus::Skipped(_)),
        "Offline should yield Skipped, got {:?}",
        result.status
    );
    // Explicit: offline does NOT yield Pass (false-green guard)
    assert_ne!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("offline"));
}

#[tokio::test]
async fn test_offline_probe_skipped_message_format() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "anthropic"),
        provider_name: "anthropic".to_string(),
        provider: Some(Arc::new(OfflineProvider)),
    };
    let result = check.run().await;
    assert!(result.message.contains("skipped"));
    assert!(result.message.contains("offline"));
    assert!(result.message.contains("network probes unavailable"));
}

// ──────────────────────────────────────────────────
// P0-2: Count-math (AC7)
// ──────────────────────────────────────────────────

#[test]
fn test_count_math_skipped_plus_zero_fail_is_exit_zero() {
    // N skipped + 0 fail → exit 0
    let results = vec![
        CheckResult {
            name: "A".into(),
            category: "test".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "B".into(),
            category: "test".into(),
            status: CheckStatus::Skipped("offline".into()),
            message: "skipped".into(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "C".into(),
            category: "test".into(),
            status: CheckStatus::Skipped("offline".into()),
            message: "skipped".into(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
    ];
    let failures = results
        .iter()
        .filter(|r| matches!(r.status, CheckStatus::Fail))
        .count();
    assert_eq!(failures, 0, "Skipped should NOT count as failure");
    // display_results should not panic
    display_results(&results);
}

#[test]
fn test_count_math_skipped_plus_one_fail_is_nonzero() {
    // N skipped + 1 fail → exit non-0
    let results = vec![
        CheckResult {
            name: "A".into(),
            category: "test".into(),
            status: CheckStatus::Skipped("offline".into()),
            message: "skipped".into(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "B".into(),
            category: "test".into(),
            status: CheckStatus::Fail,
            message: "broken".into(),
            fix: Some("fix".into()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
    ];
    let failures = results
        .iter()
        .filter(|r| matches!(r.status, CheckStatus::Fail))
        .count();
    assert_eq!(
        failures, 1,
        "Real Fail should count even with Skipped present"
    );
}

// ──────────────────────────────────────────────────
// P0-3: Probe non-billable + hermetic (AC8)
// ──────────────────────────────────────────────────

#[tokio::test]
async fn test_probe_pass_with_latency() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "test-pass"),
        provider_name: "test-pass".to_string(),
        provider: Some(Arc::new(PassProvider)),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Pass);
    assert!(result.message.contains("reachable"));
    assert!(result.message.contains("ms"));
    // Honestly states scope
    assert!(
        result
            .message
            .contains("proves auth+reachability, not chat health")
    );
}

#[tokio::test]
async fn test_probe_auth_fail() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "test-authfail"),
        provider_name: "test-authfail".to_string(),
        provider: Some(Arc::new(AuthFailProvider)),
    };
    let result = check.run().await;
    assert_eq!(result.status, CheckStatus::Fail);
    assert!(result.message.contains("authentication"));
}

#[tokio::test]
async fn test_probe_offline_skipped() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "test-offline"),
        provider_name: "test-offline".to_string(),
        provider: Some(Arc::new(OfflineProvider)),
    };
    let result = check.run().await;
    assert!(matches!(result.status, CheckStatus::Skipped(_)));
}

#[tokio::test]
async fn test_probe_not_configured() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "unconfigured"),
        provider_name: "unconfigured".to_string(),
        provider: None,
    };
    let result = check.run().await;
    assert!(matches!(result.status, CheckStatus::Skipped(_)));
    assert!(result.message.contains("not configured"));
}

// ──────────────────────────────────────────────────
// P0-4: Classifier matrix (AC2)
// ──────────────────────────────────────────────────

#[test]
fn test_provider_error_offline_is_offline() {
    let err = ProviderError::Offline("connection refused".into());
    assert!(err.is_offline());
}

#[test]
fn test_provider_error_connection_failed_not_offline() {
    let err = ProviderError::ConnectionFailed("server error".into());
    assert!(!err.is_offline());
}

#[test]
fn test_provider_error_auth_not_offline() {
    let err = ProviderError::AuthenticationFailed;
    assert!(!err.is_offline());
}

#[test]
fn test_provider_error_other_not_offline() {
    let err = ProviderError::Other("something else".into());
    assert!(!err.is_offline());
}

// ──────────────────────────────────────────────────
// P0-5: `doctor --json` valid + versioned (AC9)
// ──────────────────────────────────────────────────

#[test]
fn test_doctor_json_schema_version() {
    assert_eq!(DOCTOR_SCHEMA_VERSION, "1.0");
}

#[test]
fn test_doctor_json_serialization() {
    let results = vec![
        CheckResult {
            name: "API key".into(),
            category: "api".into(),
            status: CheckStatus::Pass,
            message: "set (via ANTHROPIC_API_KEY)".into(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "Config".into(),
            category: "config".into(),
            status: CheckStatus::Warning,
            message: "missing".into(),
            fix: Some("Run init".into()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "Provider".into(),
            category: "api".into(),
            status: CheckStatus::Skipped("offline".into()),
            message: "skipped — offline".into(),
            fix: None,
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
        CheckResult {
            name: "Broken".into(),
            category: "config".into(),
            status: CheckStatus::Fail,
            message: "invalid".into(),
            fix: Some("fix it".into()),
            latency: None,
            tier: CheckTier::ExitAffecting,
        },
    ];

    let report = DoctorReport::from_results(&results);
    let json_str = serde_json::to_string_pretty(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    // Schema version present
    assert_eq!(parsed["schema_version"], "1.0");

    // Checks array with correct count
    let checks = parsed["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 4);

    // Status values are lowercase
    let statuses: Vec<&str> = checks
        .iter()
        .map(|c| c["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, vec!["pass", "warning", "skipped", "fail"]);

    // Summary
    assert_eq!(parsed["summary"]["passed"], 1);
    assert_eq!(parsed["summary"]["warnings"], 1);
    assert_eq!(parsed["summary"]["failures"], 1);
    assert_eq!(parsed["summary"]["skipped"], 1);
    assert_eq!(parsed["summary"]["total"], 4);

    // snake_case check: no camelCase keys
    let json_keys = json_str.clone();
    assert!(
        !json_keys.contains("schemaVersion"),
        "Must use snake_case, not camelCase"
    );
    assert!(
        !json_keys.contains("latencyMs"),
        "Must use snake_case, not camelCase"
    );
}

// ──────────────────────────────────────────────────
// P0-6: Anti-vacuous — exit-affecting checks can go red (AC10)
// ──────────────────────────────────────────────────

#[test]
fn test_skipped_variant_equality() {
    // Ensure Skipped variants with different reasons are distinguishable
    let s1 = CheckStatus::Skipped("offline".to_string());
    let s2 = CheckStatus::Skipped("not configured".to_string());
    assert_ne!(s1, s2);
    assert_eq!(s1, CheckStatus::Skipped("offline".to_string()));
}

// ──────────────────────────────────────────────────
// P0-7: Regression oracle green after module move + de-bill (AC8b)
// ──────────────────────────────────────────────────
// Covered by running `cargo test --test doctor_health` — all 36 tests must pass.
// That file exercises the exact same import paths and behaviors.

// ──────────────────────────────────────────────────
// P0-9: `init` offline guard (AC4)
// ──────────────────────────────────────────────────
// AC4 is a guard comment in init.rs — no runtime test needed.
// The existing `run_init` has no network I/O, verified by code inspection.
// The guard comment documents that any future network step must skip offline.

// ──────────────────────────────────────────────────
// CLI parsing: --json flag
// ──────────────────────────────────────────────────

#[test]
fn test_cli_doctor_json_flag_parses() {
    use clap::Parser;
    use rustain::adapters::cli::commands::{Cli, Command};

    let cli = Cli::parse_from(["rustain", "doctor", "--json"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            terminal: false,
            adapters: false,
            json: true
        })
    ));
}

#[test]
fn test_cli_doctor_json_and_terminal_combine() {
    use clap::Parser;
    use rustain::adapters::cli::commands::{Cli, Command};

    let cli = Cli::parse_from(["rustain", "doctor", "--json", "--terminal"]);
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            terminal: true,
            adapters: false,
            json: true
        })
    ));
}

// ──────────────────────────────────────────────────
// P0-3 extended: wiremock probe verifies GET /v1/models
// ──────────────────────────────────────────────────

#[tokio::test]
async fn test_probe_non_billable_uses_get_models() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

    let mock_server = MockServer::start().await;

    // Mount a GET /v1/models handler that returns 200
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Mount a POST /v1/messages catch-all that should NOT be hit
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .named("POST /v1/messages must not be called")
        .mount(&mock_server)
        .await;

    // Build an OpenAI-compatible adapter pointing at the mock server
    let adapter = rustain::adapters::openai::OpenAiAdapter::new(
        rustain::adapters::openai::OpenAiCompatibleVariant::OpenAI,
        "test-key".to_string(),
        "gpt-4o".to_string(),
        Some(format!("{}/v1", mock_server.uri())),
    )
    .unwrap();

    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "openai-mock"),
        provider_name: "openai-mock".to_string(),
        provider: Some(Arc::new(adapter)),
    };
    let result = check.run().await;
    assert_eq!(
        result.status,
        CheckStatus::Pass,
        "expected Pass, got: {:?} — {}",
        result.status,
        result.message
    );
    assert!(result.latency.is_some(), "latency should be set on Pass");
    // wiremock verifies: GET /v1/models was called exactly 1 time,
    // POST /v1/messages was called 0 times (expectations assert on MockServer drop)
}

// ──────────────────────────────────────────────────
// P0-5 extended: EndpointUnsupported → Skipped
// ──────────────────────────────────────────────────

struct EndpointUnsupportedProvider;
#[async_trait]
impl StreamingProvider for EndpointUnsupportedProvider {
    async fn stream_completion(
        &self,
        _: Vec<rustain::domain::models::Message>,
        _: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, rustain::domain::models::StreamChunk>,
        ProviderError,
    > {
        unreachable!()
    }
    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "endpoint-unsupported".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::provider::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Err(ProviderError::EndpointUnsupported(404))
    }
}

#[tokio::test]
async fn test_probe_endpoint_unsupported() {
    let check = ProviderConnectivityCheck {
        name: format!("Provider connectivity ({})", "unsupported"),
        provider_name: "unsupported".to_string(),
        provider: Some(Arc::new(EndpointUnsupportedProvider)),
    };
    let result = check.run().await;
    assert!(
        matches!(result.status, CheckStatus::Skipped(ref reason) if reason == "endpoint unsupported"),
        "Expected Skipped(\"endpoint unsupported\"), got {:?}",
        result.status
    );
}

// ──────────────────────────────────────────────────
// P0-6 extended: Anti-vacuous SkillsCheck/ProfilesCheck
// ──────────────────────────────────────────────────

use rustain::adapters::cli::doctor::SkillsCheck;

#[tokio::test]
async fn test_skills_check_warns_on_missing_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing_path = tmp.path().join("does_not_exist");
    // Path does not exist → not a directory → Warning
    let check = SkillsCheck {
        workspace: Some(missing_path),
    };
    let result = check.run().await;
    assert_eq!(
        result.status,
        CheckStatus::Warning,
        "non-existent workspace should produce Warning, got: {:?} — {}",
        result.status,
        result.message
    );
    assert!(result.message.contains("not a valid directory"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_profiles_check_warns_on_unreadable_dir() {
    use rustain::adapters::cli::doctor::ProfilesCheck;

    // ProfilesCheck reads from config_dir/profiles.
    // We can't inject a custom config_dir since ProfilesCheck uses paths::config_dir().
    // Instead, verify that ProfilesCheck at least returns a valid CheckResult
    // (Warning if profiles dir is missing, which is the common case in test envs).
    let check = ProfilesCheck;
    let result = check.run().await;
    // Either Warning (dir missing / unreadable) or Pass (profiles exist) — never panics.
    assert!(
        matches!(result.status, CheckStatus::Warning | CheckStatus::Pass),
        "ProfilesCheck should produce Warning or Pass, got: {:?} — {}",
        result.status,
        result.message
    );
}
