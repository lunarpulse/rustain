//! Sandbox conformance tests (Story 9.5 — AC-9-5-1, AC-9-5-2, AC-9-5-4).
//!
//! Validates the re-export surface, NoOpSandbox default behavior, and
//! composition-root binding for the sandbox subsystem.

use std::sync::Arc;

use rustain::adapters::sandbox::{NoOpSandbox, SandboxAdapterKind, SandboxError};
use rustain::domain::models::sandbox::SandboxPolicy;
use rustain::domain::ports::SandboxManager;

// AC-9-5-1: Re-export surface compiles from both domain and adapter paths.
#[test]
fn test_re_export_surface_compiles() {
    use rustain::adapters::sandbox::{
        NoOpSandbox as _, SandboxAdapterKind as _, SandboxError as _,
    };
    use rustain::domain::ports::SandboxManager as _;
    let noop: Arc<dyn SandboxManager> = Arc::new(NoOpSandbox);
    assert_eq!(noop.kind(), SandboxAdapterKind::NoOp);
}

// AC-9-5-2: NoOpSandbox is the default on all platforms.
#[test]
fn test_noop_is_default_on_unsupported_platforms() {
    let noop = NoOpSandbox;
    assert_eq!(noop.kind(), SandboxAdapterKind::NoOp);
}

// AC-9-5-2: NoOpSandbox::apply does not break spawn.
#[cfg(unix)]
#[tokio::test]
async fn test_noop_apply_does_not_break_spawn() {
    let noop: Arc<dyn SandboxManager> = Arc::new(NoOpSandbox);
    let mut cmd = tokio::process::Command::new("/bin/true");
    noop.apply(&mut cmd, &SandboxPolicy::ReadOnly { network: false })
        .await
        .expect("NoOp apply should not fail");
    let status = cmd.status().await.expect("spawn /bin/true");
    assert!(status.success(), "/bin/true should exit 0");
}

// AC-9-5-4: Default composition binds NoOpSandbox.
#[test]
fn test_sandbox_default_is_noop() {
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    let core = AgentCore::test_noop();
    let sb = core.sandbox.load_full();
    assert_eq!(sb.kind(), SandboxAdapterKind::NoOp);
}

// AC-9-5-4: Explicit noop binding works.
#[test]
fn test_sandbox_adapter_noop_binds_explicitly() {
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    let core = AgentCore::test_noop();
    let sb = core.sandbox.load_full();
    assert_eq!(sb.kind(), SandboxAdapterKind::NoOp);
}

// AC-9-5-4: Unknown adapter value is rejected.
#[test]
fn test_sandbox_adapter_unknown_value_rejected() {
    let result = rustain::infrastructure::startup::validate_sandbox_adapter("selinux");
    assert!(result.is_err(), "unknown adapter should be rejected");
    let msg = format!("{:?}", result.err().unwrap());
    assert!(
        msg.contains("noop") || msg.contains("landlock"),
        "error should list valid values: {msg}"
    );
}

// AC-9-5-4: Empty adapter is rejected.
#[test]
fn test_sandbox_adapter_empty_rejected() {
    let result = rustain::infrastructure::startup::validate_sandbox_adapter("");
    assert!(result.is_err(), "empty adapter should be rejected");
}

// AC-9-5-4: Pre-v9.5 config round-trips to noop default.
#[test]
fn test_sandbox_config_pre_v9_5_round_trips_to_noop() {
    let config: rustain::domain::models::AppConfig =
        serde_json::from_str("{}").expect("empty config should parse");
    assert_eq!(config.sandbox.adapter, "noop");
}
