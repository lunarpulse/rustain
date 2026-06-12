//! Concrete SecurityPort adapter.
//! Wraps blocklist, path validation, and mode management.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;

use crate::domain::errors::PermissionError;
use crate::domain::models::{FileContextProvenance, FileOperation, PathAccessType, PermissionMode};
use crate::domain::ports::SecurityPort;

pub struct SecurityAdapter {
    workspace_path: PathBuf,
    blocked_commands: Vec<String>,
    blocked_paths: Vec<String>,
    mode: Arc<AtomicU8>,
    /// Active skill directories that are readable regardless of workspace boundary (AC7).
    ///
    /// CONFORMANCE_EXCEPTION_STD_SYNC_LOCK (PERMANENT, ADR-07-01): held only for
    /// short critical sections (HashSet::contains/insert/remove on a small set,
    /// ≤1µs), never across `.await`. Verified by audit of all 26 call sites.
    /// Migration to `tokio::sync::RwLock` would propagate `async fn` through
    /// `PermissionChain::check` into the synchronous tool-execution path
    /// (ADR-06-02 ToolCall FSM stability concern) — migration cost exceeds
    /// runtime benefit. This exception is INTENTIONAL and PERMANENT, not a
    /// to-fix item. See ADR-07-01 (Epic 7 retro AI-7.5, closes DF-158).
    active_skill_dirs: std::sync::RwLock<std::collections::HashSet<PathBuf>>, // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-07-01 (closes DF-158); short critical sections, never across await.
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
            mode: Arc::new(AtomicU8::new(PermissionMode::Normal as u8)),
            active_skill_dirs: std::sync::RwLock::new(std::collections::HashSet::new()), // CONFORMANCE_EXCEPTION_STD_SYNC_LOCK: PERMANENT per ADR-07-01 (closes DF-158); short critical sections, never across await.
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
    fn validate_path(
        &self,
        path: &Path,
        op: FileOperation,
        provenance: FileContextProvenance,
    ) -> Result<PathAccessType, PermissionError> {
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(PermissionError::WorkspaceViolation(
                "Path traversal not allowed".to_string(),
            ));
        }

        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_path.join(path)
        };

        let workspace_canonical = std::fs::canonicalize(&self.workspace_path)
            .unwrap_or_else(|_| self.workspace_path.clone());

        // Apply blocklist to the requested path even before canonicalization so
        // user-provided paths cannot name blocked prefixes by a non-existent path.
        let absolute_str = absolute.to_string_lossy();
        for blocked in &self.blocked_paths {
            if absolute_str.starts_with(blocked) {
                return Err(PermissionError::WorkspaceViolation(format!(
                    "Access to {} is not allowed",
                    blocked
                )));
            }
        }

        let resolved = match std::fs::canonicalize(&absolute) {
            Ok(p) => p,
            Err(_) => {
                if provenance == FileContextProvenance::UserProvided {
                    return Ok(PathAccessType::External);
                }
                if let Some(parent) = absolute.parent() {
                    match std::fs::canonicalize(parent) {
                        Ok(canon_parent) => {
                            if !canon_parent.starts_with(&workspace_canonical) {
                                return Err(PermissionError::WorkspaceViolation(
                                    "Parent directory resolves outside workspace".to_string(),
                                ));
                            }
                            if let Some(file_name) = absolute.file_name() {
                                let resolved = canon_parent.join(file_name);
                                if !resolved.starts_with(&canon_parent) {
                                    return Err(PermissionError::WorkspaceViolation(
                                        "Path traversal detected via symlink".to_string(),
                                    ));
                                }
                                match std::fs::canonicalize(&resolved) {
                                    Ok(final_resolved) => {
                                        if !final_resolved.starts_with(&workspace_canonical) {
                                            return Err(PermissionError::WorkspaceViolation(
                                                "Symlink escape detected".to_string(),
                                            ));
                                        }
                                        final_resolved
                                    }
                                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => resolved,
                                    Err(_) => {
                                        return Err(PermissionError::WorkspaceViolation(
                                            "Path resolution failed — rejecting for safety"
                                                .to_string(),
                                        ));
                                    }
                                }
                            } else {
                                canon_parent
                            }
                        }
                        Err(_) => {
                            return Err(PermissionError::WorkspaceViolation(format!(
                                "Path '{}' not in workspace — parent does not exist",
                                path.display()
                            )));
                        }
                    }
                } else {
                    return Err(PermissionError::WorkspaceViolation(format!(
                        "Path '{}' not in workspace — no parent directory",
                        path.display()
                    )));
                }
            }
        };

        let resolved_str = resolved.to_string_lossy();

        for blocked in &self.blocked_paths {
            if resolved_str.starts_with(blocked) {
                return Err(PermissionError::WorkspaceViolation(format!(
                    "Access to {} is not allowed",
                    blocked
                )));
            }
        }

        if resolved.starts_with(&workspace_canonical) {
            Ok(PathAccessType::Workspace)
        } else if provenance == FileContextProvenance::UserProvided && op == FileOperation::Read {
            Ok(PathAccessType::External)
        } else if op == FileOperation::Read {
            if let Ok(dirs) = self.active_skill_dirs.read() {
                for skill_dir in dirs.iter() {
                    if resolved.starts_with(skill_dir) {
                        return Ok(PathAccessType::Workspace);
                    }
                }
            }
            Err(PermissionError::WorkspaceViolation(
                "Path outside workspace".to_string(),
            ))
        } else {
            Err(PermissionError::WorkspaceViolation(
                "Path outside workspace".to_string(),
            ))
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
        op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        self.validate_path(path, op, FileContextProvenance::ModelSuggested)
    }

    fn check_workspace_access_with_provenance(
        &self,
        path: &Path,
        op: FileOperation,
        provenance: FileContextProvenance,
    ) -> Result<PathAccessType, PermissionError> {
        self.validate_path(path, op, provenance)
    }

    fn add_active_skill_dir(&self, dir: PathBuf) {
        let canonical = std::fs::canonicalize(&dir).unwrap_or(dir);
        if let Ok(mut dirs) = self.active_skill_dirs.write() {
            dirs.insert(canonical);
        }
    }

    fn remove_active_skill_dir(&self, dir: &Path) {
        let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if let Ok(mut dirs) = self.active_skill_dirs.write() {
            dirs.remove(&canonical);
        }
    }

    fn current_mode(&self) -> PermissionMode {
        match self.mode.load(Ordering::Acquire) {
            0 => PermissionMode::Normal,
            1 => PermissionMode::Yolo,
            2 => PermissionMode::Plan,
            3 => PermissionMode::AutoEdit,
            _ => PermissionMode::Normal,
        }
    }

    fn set_mode(&self, mode: PermissionMode) {
        let val = match mode {
            PermissionMode::Normal => 0u8,
            PermissionMode::Yolo => 1u8,
            PermissionMode::Plan => 2u8,
            PermissionMode::AutoEdit => 3u8,
        };
        self.mode.store(val, Ordering::Release);
    }
}
/// Headless security adapter — permanently Normal; `set_mode` is a no-op.
/// Used by the daemon so Yolo can never leak via a future call site (AC6).
pub struct HeadlessSecurityAdapter(SecurityAdapter);

impl HeadlessSecurityAdapter {
    pub fn new(workspace_path: PathBuf) -> Self {
        Self(SecurityAdapter::new(workspace_path))
    }
}

#[async_trait]
impl SecurityPort for HeadlessSecurityAdapter {
    fn check_blocklist(&self, command: &str) -> Result<(), PermissionError> {
        self.0.check_blocklist(command)
    }
    fn check_workspace_access(
        &self,
        path: &Path,
        op: FileOperation,
    ) -> Result<PathAccessType, PermissionError> {
        self.0.check_workspace_access(path, op)
    }
    fn check_workspace_access_with_provenance(
        &self,
        path: &Path,
        op: FileOperation,
        provenance: FileContextProvenance,
    ) -> Result<PathAccessType, PermissionError> {
        self.0
            .check_workspace_access_with_provenance(path, op, provenance)
    }
    fn add_active_skill_dir(&self, dir: PathBuf) {
        self.0.add_active_skill_dir(dir)
    }
    fn remove_active_skill_dir(&self, dir: &Path) {
        self.0.remove_active_skill_dir(dir)
    }
    fn current_mode(&self) -> PermissionMode {
        PermissionMode::Normal
    }
    fn set_mode(&self, _mode: PermissionMode) {
        tracing::debug!("headless security: set_mode ignored (permanently Normal)");
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
    fn test_workspace_deeply_nested_nonexistent_path_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter = SecurityAdapter::new(tmp.path().to_path_buf());
        let path = tmp.path().join("deep").join("missing").join("file.txt");
        let result = adapter.check_workspace_access(&path, FileOperation::Write);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not in workspace"),
            "Error should be user-friendly: {}",
            err_msg
        );
    }

    #[test]
    fn test_workspace_rejects_path_escaping_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let adapter = SecurityAdapter::new(tmp.path().to_path_buf());
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("..").join("..").join("etc").join("passwd");
        let result = adapter.check_workspace_access(&path, FileOperation::Write);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("outside workspace") || err_msg.contains("traversal"));
    }

    #[test]
    fn test_active_skill_dir_grants_read_then_revokes_on_remove() {
        let workspace = tempfile::TempDir::new().unwrap();
        let skill_root = tempfile::TempDir::new().unwrap();
        let skill_dir = skill_root.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_file = skill_dir.join("helper.md");
        std::fs::write(&skill_file, "content").unwrap();

        let adapter = SecurityAdapter::new(workspace.path().to_path_buf());

        assert!(
            adapter
                .check_workspace_access(&skill_file, FileOperation::Read)
                .is_err()
        );
        assert!(
            adapter
                .check_workspace_access(&skill_file, FileOperation::Write)
                .is_err()
        );

        adapter.add_active_skill_dir(skill_dir.clone());
        let read = adapter.check_workspace_access(&skill_file, FileOperation::Read);
        assert!(read.is_ok());

        let write = adapter.check_workspace_access(&skill_file, FileOperation::Write);
        assert!(write.is_err());

        let sibling = skill_root.path().join("not-a-skill.md");
        std::fs::write(&sibling, "x").unwrap();
        assert!(
            adapter
                .check_workspace_access(&sibling, FileOperation::Read)
                .is_err()
        );

        adapter.remove_active_skill_dir(&skill_dir);
        assert!(
            adapter
                .check_workspace_access(&skill_file, FileOperation::Read)
                .is_err()
        );
    }

    #[test]
    fn test_permission_mode_switching() {
        let adapter = make_adapter();
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
        adapter.set_mode(PermissionMode::Yolo);
        assert_eq!(adapter.current_mode(), PermissionMode::Yolo);
        adapter.set_mode(PermissionMode::Normal);
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
    }
}

#[cfg(test)]
mod headless_tests {
    use super::*;

    /// AC6: HeadlessSecurityAdapter is permanently Normal; set_mode is a no-op.
    #[test]
    fn headless_security_never_yolo() {
        let adapter = HeadlessSecurityAdapter::new(PathBuf::from("/tmp"));
        assert_ne!(adapter.current_mode(), PermissionMode::Yolo);
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
        adapter.set_mode(PermissionMode::Yolo);
        assert_ne!(adapter.current_mode(), PermissionMode::Yolo);
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
    }
}
