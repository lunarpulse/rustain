#![cfg(unix)]
//! Story Q-6 AC2 — pathname confinement at the permission boundary and at the
//! built-in `Write`/`Edit` executor.
//!
//! Two doors, one rule (`adapters::path_confinement::confine`):
//! - K2a — `ToolScheduler` → `permission_chain` → real `SecurityAdapter`, with the
//!   real `ToolSetAdapter` behind it; and restricted Bash redirects through the
//!   same chain;
//! - K2b — the executor called directly (`ToolSetPort::execute`), as a caller that
//!   skipped the chain would, with a real checkpoint context so a snapshot would
//!   be observable. The inert I/O observer is the storage directory: a rejected
//!   call must leave no snapshot file, no created target and no read-side effect.
//!
//! Fixtures are ephemeral tempdirs. A dangling link's target is a sibling path
//! that must stay absent; no sensitive file or real external path is involved.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustain::adapters::noop::NoOpToolSet;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{PermissionMode, ProvenanceTag, SkillToolRestriction, ToolCall};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::permission_chain::{self, PermissionDecision};
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

const LEAF_REFUSAL: &str = "Path leaf is a link or could not be resolved";

struct Fixture {
    scheduler: Arc<ToolScheduler>,
    tools: Arc<ToolSetAdapter>,
    security: Arc<SecurityAdapter>,
    /// `<base>/ws`
    workspace: PathBuf,
    /// `<base>/out` — a sibling, outside the workspace.
    outside: PathBuf,
    /// Where checkpoint snapshots land; empty means no snapshot was taken.
    sessions: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn fixture(mode: PermissionMode) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    let outside = base.join("out");
    let sessions = base.join("sessions");
    for dir in [&workspace, &outside, &sessions] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::create_dir_all(workspace.join("sub")).unwrap();

    let security = Arc::new(SecurityAdapter::new(workspace.clone()));
    security.set_mode(mode);
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(sessions.clone()),
    );
    let tools = Arc::new(ToolSetAdapter::new(
        workspace.clone(),
        storage,
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    ));
    tools
        .set_execution_context("conv-q6-k2".to_string(), CheckpointId(1), 0)
        .await;
    let approvals = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(
        security.clone(),
        tools.clone() as Arc<dyn ToolSetPort>,
        approvals,
        16,
    );
    Fixture {
        scheduler,
        tools,
        security,
        workspace,
        outside,
        sessions,
        _tmp: tmp,
    }
}

fn files_under(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                if path.is_dir() { files_under(&path) } else { 1 }
            })
            .sum()
    })
}

async fn run(fixture: &Fixture, tool: &str, input: serde_json::Value) -> ToolCall {
    run_restricted(fixture, tool, input, None).await
}

async fn run_restricted(
    fixture: &Fixture,
    tool: &str,
    input: serde_json::Value,
    skill: Option<&SkillToolRestriction>,
) -> ToolCall {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        fixture
            .scheduler
            .clone()
            .schedule_with_provenance_and_restriction(
                ApprovalSource::ForegroundTurn {
                    conversation_id: "conv-q6-k2".to_string(),
                },
                vec![ToolCallRequest {
                    id: "call-q6".to_string(),
                    tool_name: tool.to_string(),
                    input,
                }],
                CancellationToken::new(),
                skill,
                None,
                ProvenanceTag::UserOriginated,
                None,
            ),
    )
    .await
    .expect("a hard denial or an approved call; never an approval card in Yolo")
    .remove(0)
}

fn error_text(call: ToolCall) -> String {
    match call {
        ToolCall::Error { error, .. } => error,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Dangling links (one resolving inside the workspace, one outside) and an
/// escaping directory link, built per fixture.
fn link_fixtures(fixture: &Fixture) {
    let ws = &fixture.workspace;
    std::os::unix::fs::symlink(ws.join("absent-inside"), ws.join("dangling-inside")).unwrap();
    std::os::unix::fs::symlink(
        fixture.outside.join("absent-outside"),
        ws.join("dangling-outside"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&fixture.outside, ws.join("escape")).unwrap();
}

// ───────────────────────────── K2a — permission boundary ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k2a_write_and_edit_positive_controls_still_work_in_yolo() {
    let fx = fixture(PermissionMode::Yolo).await;
    std::fs::write(fx.workspace.join("sub/existing.txt"), "alpha\nbeta\n").unwrap();

    let created = run(
        &fx,
        "Write",
        serde_json::json!({ "file_path": "sub/new.txt", "content": "hello\n" }),
    )
    .await;
    assert!(matches!(created, ToolCall::Success { .. }), "{created:?}");
    assert_eq!(
        std::fs::read_to_string(fx.workspace.join("sub/new.txt")).unwrap(),
        "hello\n"
    );

    let edited = run(
        &fx,
        "Edit",
        serde_json::json!({
            "file_path": "sub/existing.txt",
            "old_string": "beta",
            "new_string": "gamma",
        }),
    )
    .await;
    assert!(matches!(edited, ToolCall::Success { .. }), "{edited:?}");
    assert_eq!(
        std::fs::read_to_string(fx.workspace.join("sub/existing.txt")).unwrap(),
        "alpha\ngamma\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn k2a_dangling_unverified_and_escaping_leaves_are_denied_before_dispatch_in_every_mode() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let fx = fixture(mode).await;
        link_fixtures(&fx);
        let cases: [(&str, &str, &Path); 4] = [
            (
                "dangling-inside",
                LEAF_REFUSAL,
                &fx.workspace.join("absent-inside"),
            ),
            (
                "dangling-outside",
                LEAF_REFUSAL,
                &fx.outside.join("absent-outside"),
            ),
            (
                "escape/new.txt",
                "Parent directory resolves outside workspace",
                &fx.outside.join("new.txt"),
            ),
            (
                "no-such-dir/new.txt",
                "parent does not exist",
                &fx.workspace.join("no-such-dir/new.txt"),
            ),
        ];
        for (target, reason, witness) in cases {
            for tool in ["Write", "Edit"] {
                let input = if tool == "Write" {
                    serde_json::json!({ "file_path": target, "content": "x" })
                } else {
                    serde_json::json!({
                        "file_path": target, "old_string": "a", "new_string": "b",
                    })
                };
                let text = error_text(run(&fx, tool, input).await);
                assert!(
                    text.contains(reason),
                    "{mode:?}/{tool}/{target}: expected {reason:?}, got {text:?}"
                );
                assert!(
                    !witness.exists(),
                    "{mode:?}/{tool}/{target}: target created"
                );
            }
        }
        assert_eq!(
            files_under(&fx.sessions),
            0,
            "{mode:?}: no snapshot was taken"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn k2a_a_dangling_leaf_is_denied_for_an_approved_restricted_redirect_but_ordinary_leaves_run()
{
    let fx = fixture(PermissionMode::Yolo).await;
    link_fixtures(&fx);
    let skill = SkillToolRestriction {
        skill_names: vec!["q6".to_string()],
        items: ["Bash(echo:*)".to_string()].into_iter().collect(),
        inherited: false,
    };

    let ordinary = run_restricted(
        &fx,
        "Bash",
        serde_json::json!({ "command": "echo ok > sub/ordinary.txt" }),
        Some(&skill),
    )
    .await;
    assert!(matches!(ordinary, ToolCall::Success { .. }), "{ordinary:?}");
    assert_eq!(
        std::fs::read_to_string(fx.workspace.join("sub/ordinary.txt")).unwrap(),
        "ok\n"
    );

    for (leaf, witness) in [
        ("dangling-inside", fx.workspace.join("absent-inside")),
        ("dangling-outside", fx.outside.join("absent-outside")),
    ] {
        let text = error_text(
            run_restricted(
                &fx,
                "Bash",
                serde_json::json!({ "command": format!("echo ok > {leaf}") }),
                Some(&skill),
            )
            .await,
        );
        assert!(text.contains(LEAF_REFUSAL), "{leaf}: {text:?}");
        assert!(!witness.exists(), "{leaf}: the redirect target was created");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn k2a_trusted_external_read_behavior_is_unchanged() {
    // A user-provided path that does not exist is still an external-read
    // candidate; the dangling rule is the model-suggested path's.
    let fx = fixture(PermissionMode::Normal).await;
    let missing = fx.outside.join("user-provided.txt");
    let verdict = fx
        .security
        .check_workspace_access_with_provenance(
            &missing,
            rustain::domain::models::FileOperation::Read,
            rustain::domain::models::FileContextProvenance::UserProvided,
        )
        .expect("user-provided external read keeps its meaning");
    assert_eq!(verdict, rustain::domain::models::PathAccessType::External);
    // And an ordinary existing in-workspace file is still a workspace read.
    std::fs::write(fx.workspace.join("sub/readable.txt"), "r").unwrap();
    assert_eq!(
        fx.security
            .check_workspace_access(
                Path::new("sub/readable.txt"),
                rustain::domain::models::FileOperation::Read
            )
            .unwrap(),
        rustain::domain::models::PathAccessType::Workspace
    );
}

// ───────────────────────────── K2b — executor boundary ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn k2b_the_executor_rejects_without_read_snapshot_or_write_and_still_serves_ordinary_paths() {
    let fx = fixture(PermissionMode::Yolo).await;
    link_fixtures(&fx);

    // Positive control on the same door: an ordinary write snapshots and writes.
    let ok = fx
        .tools
        .execute(
            "Write",
            serde_json::json!({ "file_path": "sub/fine.txt", "content": "fine\n" }),
            CancellationToken::new(),
        )
        .await
        .expect("an in-workspace creation is served");
    assert!(!ok.is_error, "{ok:?}");
    assert!(
        files_under(&fx.sessions) > 0,
        "control: a snapshot is observable"
    );
    let baseline = files_under(&fx.sessions);

    for (target, reason, witness) in [
        (
            "dangling-inside",
            LEAF_REFUSAL,
            fx.workspace.join("absent-inside"),
        ),
        (
            "dangling-outside",
            LEAF_REFUSAL,
            fx.outside.join("absent-outside"),
        ),
        (
            "escape/new.txt",
            "Parent directory resolves outside workspace",
            fx.outside.join("new.txt"),
        ),
        (
            "../traversal.txt",
            "traversal",
            fx.workspace.join("../traversal.txt"),
        ),
    ] {
        for tool in ["Write", "Edit"] {
            let input = if tool == "Write" {
                serde_json::json!({ "file_path": target, "content": "x" })
            } else {
                serde_json::json!({ "file_path": target, "old_string": "a", "new_string": "b" })
            };
            let error = fx
                .tools
                .execute(tool, input, CancellationToken::new())
                .await
                .expect_err(&format!("{tool}/{target} must be rejected"))
                .to_string();
            assert!(error.contains(reason), "{tool}/{target}: {error:?}");
            assert!(
                !error.contains("Failed to read"),
                "{tool}/{target}: the Edit read ran before confinement"
            );
            assert!(!witness.exists(), "{tool}/{target}: a target was created");
        }
    }
    assert_eq!(
        files_under(&fx.sessions),
        baseline,
        "a rejected call takes no checkpoint snapshot"
    );
}

#[test]
fn k2_a_chain_decision_for_a_dangling_leaf_is_a_deny_not_a_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = std::fs::canonicalize(tmp.path()).unwrap();
    std::os::unix::fs::symlink(ws.join("nowhere"), ws.join("dangling")).unwrap();
    let security = SecurityAdapter::new(ws.clone());
    security.set_mode(PermissionMode::Normal);
    let decision = futures::executor::block_on(
        permission_chain::check_with_source_and_provenance_and_restriction(
            &security,
            "Write",
            &serde_json::json!({ "file_path": "dangling", "content": "x" }),
            None,
            None,
            None,
            &NoOpToolSet,
            None,
            ProvenanceTag::UserOriginated,
            None,
        ),
    );
    match decision {
        PermissionDecision::Deny(reason) => assert!(reason.contains(LEAF_REFUSAL), "{reason}"),
        other => panic!("expected Deny, got {other:?}"),
    }
}
