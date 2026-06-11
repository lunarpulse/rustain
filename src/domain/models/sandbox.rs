use serde::{Deserialize, Serialize};
use std::path::Path;

use super::PermissionMode;

/// Sandbox policy derived from the current permission mode.
/// Auto-computed at session start and on every mode change.
/// Actual Landlock enforcement is Story 9-5; this story delivers the type + derivation only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxPolicy {
    /// No restrictions (YOLO mode).
    Permissive,
    /// Read-only filesystem, optionally with network blocked.
    ReadOnly { network: bool },
    /// Writable within workspace roots, with specific read-only paths.
    WorkspaceWrite {
        writable_roots: Vec<std::path::PathBuf>,
        read_only_paths: Vec<std::path::PathBuf>,
        network: bool,
    },
}

impl SandboxPolicy {
    /// Derive the sandbox policy from the current permission mode and workspace path.
    pub fn from_mode(mode: PermissionMode, workspace: &Path) -> Self {
        match mode {
            PermissionMode::Plan => SandboxPolicy::ReadOnly { network: false },
            PermissionMode::Normal | PermissionMode::AutoEdit => SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![workspace.to_path_buf()],
                read_only_paths: vec![workspace.join(".git"), workspace.join(".rustain")],
                network: true,
            },
            PermissionMode::Yolo => SandboxPolicy::Permissive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_mode_plan_is_readonly_no_network() {
        let ws = std::path::Path::new("/tmp/ws");
        assert_eq!(
            SandboxPolicy::from_mode(PermissionMode::Plan, ws),
            SandboxPolicy::ReadOnly { network: false }
        );
    }

    #[test]
    fn from_mode_normal_is_workspace_write() {
        let ws = std::path::Path::new("/tmp/ws");
        let policy = SandboxPolicy::from_mode(PermissionMode::Normal, ws);
        match policy {
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                read_only_paths,
                network,
            } => {
                assert_eq!(writable_roots, vec![ws.to_path_buf()]);
                assert_eq!(read_only_paths, vec![ws.join(".git"), ws.join(".rustain")]);
                assert!(network);
            }
            other => panic!("expected WorkspaceWrite, got {:?}", other),
        }
    }

    #[test]
    fn from_mode_yolo_is_permissive() {
        let ws = std::path::Path::new("/tmp/ws");
        assert_eq!(
            SandboxPolicy::from_mode(PermissionMode::Yolo, ws),
            SandboxPolicy::Permissive
        );
    }
}
