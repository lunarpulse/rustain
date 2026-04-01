//! Concrete SecurityPort adapter.
//! Wraps the same blocklist and path validation logic as rustycode's SecurityValidator.
//! For this story: request_permission always returns Allow (YOLO-like behavior).
//! Story 1-6 adds the full permission prompt flow.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;

use crate::domain::errors::PermissionError;
use crate::domain::models::{ApprovalDecision, FileOperation, PathAccessType, PermissionMode};
use crate::domain::ports::SecurityPort;

/// SecurityPort implementation with blocklist enforcement and workspace boundary checks.
pub struct SecurityAdapter {
    workspace_path: PathBuf,
    blocked_commands: Vec<String>,
    blocked_paths: Vec<String>,
    mode: Arc<AtomicU8>,
}

impl SecurityAdapter {
    pub fn new(workspace_path: PathBuf) -> Self {
        let blocked_commands = vec![
            "rm -rf /".to_string(),
            "dd if=/dev/zero".to_string(),
            "mkfs".to_string(),
            ":(){ :|:& };:".to_string(),
            "> /dev/sda".to_string(),
            "chmod -R 777 /".to_string(),
            "chown -R".to_string(),
            "sudo rm".to_string(),
            "sudo dd".to_string(),
        ];

        let blocked_paths = vec![
            "/etc/".to_string(),
            "/bin/".to_string(),
            "/usr/".to_string(),
            "/sys/".to_string(),
            "/proc/".to_string(),
            "/dev/".to_string(),
            "/boot/".to_string(),
            "/root/".to_string(),
        ];

        Self {
            workspace_path,
            blocked_commands,
            blocked_paths,
            mode: Arc::new(AtomicU8::new(PermissionMode::Yolo as u8)),
        }
    }

    /// Validate a shell command against the blocklist.
    fn validate_command(&self, command: &str) -> Result<(), PermissionError> {
        let command_lower = command.to_lowercase();

        for blocked in &self.blocked_commands {
            if command_lower.contains(&blocked.to_lowercase()) {
                return Err(PermissionError::Blocked(format!(
                    "Command blocked: dangerous pattern '{}'",
                    blocked
                )));
            }
        }

        let suspicious_patterns = [
            "&& rm -rf",
            "; rm -rf",
            "| rm -rf",
            "`rm -rf",
            "$(rm -rf",
            ">/dev/sd",
            ">/dev/nvme",
            "&>/dev/sd",
        ];

        for pattern in &suspicious_patterns {
            if command_lower.contains(pattern) {
                return Err(PermissionError::Blocked(format!(
                    "Suspicious command pattern detected: {}",
                    pattern
                )));
            }
        }

        Ok(())
    }

    /// Validate a file path against workspace boundaries and blocklist.
    fn validate_path(&self, path: &Path) -> Result<PathAccessType, PermissionError> {
        // Check for path traversal using component analysis (not substring matching)
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(PermissionError::WorkspaceViolation(
                "Path traversal not allowed".to_string(),
            ));
        }

        // Resolve to absolute path
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_path.join(path)
        };

        // Use canonicalize if the path exists, otherwise canonicalize the parent
        // to resolve symlinks even for non-existent paths
        let resolved = match std::fs::canonicalize(&absolute) {
            Ok(p) => p,
            Err(_) => {
                // Path doesn't exist yet (Write creating new file) — canonicalize parent
                if let Some(parent) = absolute.parent() {
                    match std::fs::canonicalize(parent) {
                        Ok(canon_parent) => {
                            if let Some(file_name) = absolute.file_name() {
                                canon_parent.join(file_name)
                            } else {
                                canon_parent
                            }
                        }
                        Err(_) => absolute.components().collect::<PathBuf>(),
                    }
                } else {
                    absolute.components().collect::<PathBuf>()
                }
            }
        };

        let resolved_str = resolved.to_string_lossy();

        // Check against blocked system paths
        for blocked in &self.blocked_paths {
            if resolved_str.starts_with(blocked) {
                return Err(PermissionError::WorkspaceViolation(format!(
                    "Access to {} is not allowed",
                    blocked
                )));
            }
        }

        // Check if within workspace
        let workspace_canonical = std::fs::canonicalize(&self.workspace_path)
            .unwrap_or_else(|_| self.workspace_path.clone());

        if resolved.starts_with(&workspace_canonical) {
            Ok(PathAccessType::Workspace)
        } else {
            Err(PermissionError::WorkspaceViolation(format!(
                "Path '{}' is outside workspace '{}'",
                resolved.display(),
                workspace_canonical.display()
            )))
        }
    }
}

impl std::fmt::Debug for SecurityAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityAdapter")
            .field("workspace_path", &self.workspace_path)
            .field("mode", &self.current_mode())
            .finish()
    }
}

#[async_trait]
impl SecurityPort for SecurityAdapter {
    fn check_blocklist(&self, command: &str) -> Result<(), PermissionError> {
        self.validate_command(command)
    }

    fn check_workspace_access(
        &self,
        path: &Path,
        _op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        self.validate_path(path)
    }

    async fn request_permission(
        &self,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
    ) -> Result<ApprovalDecision, PermissionError> {
        // Story 1-5: always auto-approve (YOLO-like behavior).
        // Story 1-6 adds the full PermissionCard prompt flow via oneshot channel.
        Ok(ApprovalDecision::Allow)
    }

    fn current_mode(&self) -> PermissionMode {
        match self.mode.load(Ordering::Relaxed) {
            0 => PermissionMode::Normal,
            _ => PermissionMode::Yolo,
        }
    }

    fn set_mode(&self, mode: PermissionMode) {
        let val = match mode {
            PermissionMode::Normal => 0u8,
            PermissionMode::Yolo => 1u8,
        };
        self.mode.store(val, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn make_adapter() -> SecurityAdapter {
        SecurityAdapter::new(env::current_dir().unwrap())
    }

    #[test]
    fn test_blocklist_catches_rm_rf() {
        let adapter = make_adapter();
        assert!(adapter.check_blocklist("rm -rf /").is_err());
    }

    #[test]
    fn test_blocklist_catches_fork_bomb() {
        let adapter = make_adapter();
        assert!(adapter.check_blocklist(":(){ :|:& };:").is_err());
    }

    #[test]
    fn test_blocklist_catches_dd() {
        let adapter = make_adapter();
        assert!(
            adapter
                .check_blocklist("dd if=/dev/zero of=/dev/sda")
                .is_err()
        );
    }

    #[test]
    fn test_blocklist_allows_safe_commands() {
        let adapter = make_adapter();
        assert!(adapter.check_blocklist("echo hello").is_ok());
        assert!(adapter.check_blocklist("ls -la").is_ok());
        assert!(adapter.check_blocklist("cargo test").is_ok());
    }

    #[test]
    fn test_blocklist_catches_suspicious_patterns() {
        let adapter = make_adapter();
        assert!(adapter.check_blocklist("ls && rm -rf /").is_err());
        assert!(adapter.check_blocklist("cat file; rm -rf /").is_err());
        assert!(adapter.check_blocklist("echo | rm -rf /").is_err());
    }

    #[test]
    fn test_workspace_rejects_path_traversal() {
        let adapter = make_adapter();
        let result =
            adapter.check_workspace_access(Path::new("../../../etc/passwd"), FileOperation::Read);
        assert!(result.is_err());
    }

    #[test]
    fn test_workspace_allows_relative_path() {
        let adapter = make_adapter();
        let result =
            adapter.check_workspace_access(Path::new("./src/main.rs"), FileOperation::Read);
        assert!(result.is_ok());
    }

    #[test]
    fn test_workspace_rejects_system_paths() {
        let adapter = make_adapter();
        let result = adapter.check_workspace_access(Path::new("/etc/passwd"), FileOperation::Read);
        assert!(result.is_err());
    }

    #[test]
    fn test_permission_mode_switching() {
        let adapter = make_adapter();
        assert_eq!(adapter.current_mode(), PermissionMode::Yolo);
        adapter.set_mode(PermissionMode::Normal);
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
        adapter.set_mode(PermissionMode::Yolo);
        assert_eq!(adapter.current_mode(), PermissionMode::Yolo);
    }

    #[tokio::test]
    async fn test_request_permission_always_allows() {
        let adapter = make_adapter();
        let result = adapter
            .request_permission("bash", &serde_json::json!({"command": "ls"}))
            .await;
        assert_eq!(result.unwrap(), ApprovalDecision::Allow);
    }

    #[test]
    fn test_permission_chain_blocked_overrides_allow() {
        let adapter = make_adapter();
        // Even though request_permission would Allow, blocklist should block first
        assert!(adapter.check_blocklist("rm -rf /").is_err());
    }
}
