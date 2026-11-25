//! `NoOpSandbox` — the unconfined sandbox adapter.
//!
//! Zero OS-level enforcement. `PermissionChain::check` is the only line of
//! defense in this configuration. This adapter exists so the composition
//! root always has a `SandboxManager` to bind — the absence of OS sandbox
//! support is a property of THIS ADAPTER's behavior, not of the slot being
//! empty.
//!
//! # When this ships in production (ADR-23-1-01 §6)
//!
//! - `[sandbox].adapter = "noop"` (the sandbox is off).
//! - `"auto"` on a Linux kernel without Landlock ABI v3, or a build without
//!   the `sandbox` feature (status `Unavailable`; YOLO refused).
//! - macOS and Windows (status `NotSupported`).
//! - Composition contexts that spawn no commands.

use async_trait::async_trait;
use tokio::process::Command;

use super::{SandboxAdapterKind, SandboxError};
use crate::domain::models::sandbox::SandboxPolicy;
use crate::domain::ports::SandboxManager;

/// Zero-enforcement sandbox adapter — composition default.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpSandbox;

#[async_trait]
impl SandboxManager for NoOpSandbox {
    fn kind(&self) -> SandboxAdapterKind {
        SandboxAdapterKind::NoOp
    }

    async fn apply(&self, _cmd: &mut Command, _policy: &SandboxPolicy) -> Result<(), SandboxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ws() -> PathBuf {
        PathBuf::from("/tmp/test-ws")
    }

    #[tokio::test]
    async fn test_kind_returns_noop() {
        assert_eq!(NoOpSandbox.kind(), SandboxAdapterKind::NoOp);
    }

    #[tokio::test]
    async fn test_apply_is_ok_for_all_policies() {
        let sb = NoOpSandbox;
        let mut cmd = Command::new("/bin/true");
        assert!(sb.apply(&mut cmd, &SandboxPolicy::Permissive).await.is_ok());
        assert!(
            sb.apply(&mut cmd, &SandboxPolicy::ReadOnly { network: false },)
                .await
                .is_ok()
        );
        assert!(
            sb.apply(
                &mut cmd,
                &SandboxPolicy::WorkspaceWrite {
                    writable_roots: vec![ws()],
                    read_only_paths: vec![],
                    network: true,
                },
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn test_command_unmodified_after_apply() {
        let sb = NoOpSandbox;
        let mut cmd = Command::new("/bin/true");
        sb.apply(&mut cmd, &SandboxPolicy::ReadOnly { network: false })
            .await
            .unwrap();
        let status = cmd.status().await.unwrap();
        assert!(status.success());
    }
}
