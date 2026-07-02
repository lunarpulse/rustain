//! Story 9.2 — MCP approval persistence: ``Always for [mcp__<server>]``.
//!
//! ADR-06-08 + docs/mcp.md require that an "Always for <server>" approval is
//! keyed to the **canonical** ``mcp__<server>`` identifier (not the bare server
//! name), so:
//!   * a future skill/tool/builtin named "<server>" can't piggy-back on the
//!     persisted approval, and
//!   * the next ``mcp__<server>__<any_tool>`` call auto-approves without
//!     prompting again.
//!
//! Risk closed:
//!   * R4 — ``Always for [mcp__server]`` persists + survives a fresh runtime
//!     with the same persistence backend, and binds to the canonical namespace.
//!
//! These are runtime-level tests on `ApprovalRuntime` + `ApprovalPersistenceToml`,
//! independent of the LLM or TUI. A live-LLM `@requires_api` smoke can layer
//! on top later for full E2E realism (per the per-feature LLM-smoke policy).

use std::sync::Arc;

use rustain::adapters::approval_persistence_toml::ApprovalPersistenceToml;
use rustain::domain::models::tool_call::ApprovalSource;
use rustain::domain::models::{ApprovalOutcome, ApprovalScope, ToolRisk};
use rustain::domain::ports::ApprovalPersistencePort;
use rustain::domain::services::approval_runtime::ApprovalRuntime;

/// The canonical wire form for an MCP server's approval scope. ADR-06-08:
/// approvals are keyed to ``mcp__<server>`` to namespace away from future
/// builtin/skill names that could collide on the bare server id.
const ECHO_SCOPE_KEY: &str = "mcp__echo";

#[tokio::test]
async fn r4_always_for_mcp_server_persists_to_toml_with_canonical_key() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let user_config = tmp.path().join("config.toml");
    let workspace_rules = tmp.path().join("permissions.toml");

    let persistence: Arc<dyn ApprovalPersistencePort> = Arc::new(ApprovalPersistenceToml::new(
        user_config.clone(),
        workspace_rules,
    ));
    let runtime = ApprovalRuntime::new(16, persistence);

    // First call: not yet auto-approved → slow path returns Some(id).
    let (id, rx) = runtime
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c-r4".into(),
            },
            "mcp__echo__echo".to_string(),
            serde_json::json!({"text": "hi"}),
            ToolRisk::Elevated,
            Some(ECHO_SCOPE_KEY),
            None,
        )
        .await;
    let id = id.expect("first request must be slow-path (no prior approval)");

    // User picks "Always" + save → AlwaysAndSave with Server scope.
    runtime
        .resolve(
            &id,
            ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Server(ECHO_SCOPE_KEY.into()),
            },
        )
        .await;

    // The receiver should hear the AlwaysAndSave outcome.
    let resolved = rx.await.expect("approval channel must deliver");
    assert!(
        matches!(resolved.outcome, ApprovalOutcome::AlwaysAndSave { .. }),
        "expected AlwaysAndSave outcome, got {:?}",
        resolved.outcome
    );

    // Persistence must have written the canonical scope key into the TOML.
    let written = tokio::fs::read_to_string(&user_config)
        .await
        .expect("config.toml must exist after AlwaysAndSave");
    assert!(
        written.contains(&format!("\"{ECHO_SCOPE_KEY}\"")),
        "expected canonical key '{ECHO_SCOPE_KEY}' in persisted config.toml; got:\n{written}"
    );
    assert!(
        written.contains("always_servers"),
        "expected the always_servers table key; got:\n{written}"
    );
    // Negative: the bare server name MUST NOT be the persisted key (collision risk).
    assert!(
        !written.contains("\"echo\""),
        "must persist canonical 'mcp__echo', not bare 'echo' (collision risk); got:\n{written}"
    );
}

#[tokio::test]
async fn r4_persisted_approval_auto_approves_future_tools_on_same_server() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let user_config = tmp.path().join("config.toml");
    let workspace_rules = tmp.path().join("permissions.toml");

    // Phase 1 — first runtime: user approves Always-and-save.
    {
        let persistence: Arc<dyn ApprovalPersistencePort> = Arc::new(ApprovalPersistenceToml::new(
            user_config.clone(),
            workspace_rules.clone(),
        ));
        let rt = ApprovalRuntime::new(16, persistence);
        let (id, _rx) = rt
            .request(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "c-r4a".into(),
                },
                "mcp__echo__echo".to_string(),
                serde_json::json!({}),
                ToolRisk::Elevated,
                Some(ECHO_SCOPE_KEY),
                None,
            )
            .await;
        rt.resolve(
            &id.expect("slow path"),
            ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Server(ECHO_SCOPE_KEY.into()),
            },
        )
        .await;
    }

    // Phase 2 — fresh runtime, same backing files. Loading the session must
    // bring back the persisted scope. The next request for a DIFFERENT tool
    // under the SAME server must fast-path.
    let persistence: Arc<dyn ApprovalPersistencePort> = Arc::new(ApprovalPersistenceToml::new(
        user_config.clone(),
        workspace_rules,
    ));
    let rt2 = ApprovalRuntime::new(16, persistence);
    rt2.load_session().await;

    let (id2, rx2) = rt2
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c-r4b".into(),
            },
            "mcp__echo__add".to_string(), // different tool on same server
            serde_json::json!({"a": 1, "b": 1}),
            ToolRisk::Elevated,
            Some(ECHO_SCOPE_KEY),
            None,
        )
        .await;
    assert!(
        id2.is_none(),
        "expected fast-path (no slow-path id) after persisted Always for server; got {:?}",
        id2
    );
    let resolved = rx2.await.expect("fast-path channel must deliver");
    assert!(
        matches!(resolved.outcome, ApprovalOutcome::Once),
        "fast-path outcome should be Once, got {:?}",
        resolved.outcome
    );
}

#[tokio::test]
async fn r4_persisted_approval_does_not_leak_to_other_servers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let user_config = tmp.path().join("config.toml");
    let workspace_rules = tmp.path().join("permissions.toml");

    let persistence: Arc<dyn ApprovalPersistencePort> = Arc::new(ApprovalPersistenceToml::new(
        user_config.clone(),
        workspace_rules.clone(),
    ));
    let rt = ApprovalRuntime::new(16, persistence);

    // Approve "mcp__echo" only.
    let (id, _) = rt
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c".into(),
            },
            "mcp__echo__echo".to_string(),
            serde_json::json!({}),
            ToolRisk::Elevated,
            Some(ECHO_SCOPE_KEY),
            None,
        )
        .await;
    rt.resolve(
        &id.expect("slow path"),
        ApprovalOutcome::AlwaysAndSave {
            scope: ApprovalScope::Server(ECHO_SCOPE_KEY.into()),
        },
    )
    .await;

    // A DIFFERENT server ("mcp__other") must still go through the slow path.
    let (id2, _rx2) = rt
        .request(
            ApprovalSource::ForegroundTurn {
                conversation_id: "c".into(),
            },
            "mcp__other__do_thing".to_string(),
            serde_json::json!({}),
            ToolRisk::Elevated,
            Some("mcp__other"),
            None,
        )
        .await;
    assert!(
        id2.is_some(),
        "approval for mcp__echo MUST NOT auto-approve mcp__other (namespace leak)"
    );
}
