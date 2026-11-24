//! Story Q-6 AC5 — a real blocklist refusal carries ONE `Command blocked:` prefix
//! with one meaningful reason, in both terminal projections; the native and the
//! registered-MCP routes keep their own policy.
//!
//! The deny criterion is the adapter's own benign synthetic default (`mkfs`
//! appearing in an `echo` argument): nothing dangerous is ever run, because the
//! refusal happens before dispatch, and a control proves the same command shape
//! WITHOUT the criterion is admitted and runs.

use std::sync::Arc;

use arc_swap::ArcSwap;
use rustain::adapters::noop::NoOpToolSet;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{PermissionMode, ProvenanceTag, ToolCall};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::permission_chain::{self, PermissionDecision};
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

const CRITERION_COMMAND: &str = "echo mkfs";
const BENIGN_COMMAND: &str = "echo fine";

async fn run_bash(mode: PermissionMode, tool: &str, command: &str) -> ToolCall {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let security = Arc::new(SecurityAdapter::new(workspace.clone()));
    security.set_mode(mode);
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(base.join("sessions")),
    );
    let tools: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
        workspace,
        storage,
        Arc::new(ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    ));
    let approvals = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approvals, 16);
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        scheduler.schedule_with_provenance_and_restriction(
            ApprovalSource::ForegroundTurn {
                conversation_id: "conv-q6-k5".to_string(),
            },
            vec![ToolCallRequest {
                id: "bash-q6".to_string(),
                tool_name: tool.to_string(),
                input: serde_json::json!({ "command": command }),
            }],
            CancellationToken::new(),
            None,
            None,
            ProvenanceTag::UserOriginated,
            None,
        ),
    )
    .await
    .expect("a hard denial or an admitted call; never an approval card in Yolo")
    .remove(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn k5_a_blocklist_refusal_has_one_prefix_with_one_reason_even_when_the_mode_would_allow() {
    for tool in ["Bash", "bash"] {
        let call = run_bash(PermissionMode::Yolo, tool, CRITERION_COMMAND).await;
        let ToolCall::Error { error, .. } = call else {
            panic!("{tool}: expected a refusal, got {call:?}");
        };
        // The `error` string is both projections: the rendered tool outcome and
        // the model-facing tool result are built from it unchanged.
        assert_eq!(
            error, "Command blocked: dangerous pattern 'mkfs'",
            "{tool}: one typed prefix, one reason"
        );
        assert_eq!(error.matches("Command blocked:").count(), 1, "{tool}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn k5_the_same_shape_without_the_criterion_runs_on_the_native_route_and_its_alias() {
    for tool in ["Bash", "bash"] {
        let call = run_bash(PermissionMode::Yolo, tool, BENIGN_COMMAND).await;
        let ToolCall::Success { result, .. } = call else {
            panic!("{tool}: control must be admitted, got {call:?}");
        };
        assert_eq!(result.output.trim(), "fine", "{tool}");
    }
}

async fn chain(tool: &str, mode: PermissionMode, command: &str) -> PermissionDecision {
    let tmp = tempfile::tempdir().unwrap();
    let security = SecurityAdapter::new(tmp.path().to_path_buf());
    security.set_mode(mode);
    permission_chain::check_with_source_and_provenance_and_restriction(
        &security,
        tool,
        &serde_json::json!({ "command": command }),
        None,
        None,
        None,
        &NoOpToolSet,
        None,
        ProvenanceTag::UserOriginated,
        None,
    )
    .await
}

#[tokio::test]
async fn k5_a_registered_mcp_tool_with_a_builtin_basename_keeps_its_own_route_and_scope() {
    // Not routed through the Bash blocklist step: the same criterion text is not
    // a Bash refusal here, and the approval scope stays the server's.
    let qualified = "mcp__fixture__Bash";
    match chain(qualified, PermissionMode::Normal, CRITERION_COMMAND).await {
        PermissionDecision::Prompt { server_id, .. } => {
            assert_eq!(server_id.as_deref(), Some("mcp__fixture"));
        }
        other => panic!("expected the server-scoped prompt, got {other:?}"),
    }
    // And the native route still refuses the same text with the single prefix.
    match chain("Bash", PermissionMode::Normal, CRITERION_COMMAND).await {
        PermissionDecision::Deny(reason) => {
            assert_eq!(reason, "Command blocked: dangerous pattern 'mkfs'");
        }
        other => panic!("expected a blocklist denial, got {other:?}"),
    }
}
