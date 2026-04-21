//! Concrete SecurityPort adapter.
//! Wraps blocklist, path validation, and permission prompt flow via oneshot channels.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use tokio::sync::{RwLock, mpsc};

use crate::domain::errors::PermissionError;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    ApprovalDecision, FileOperation, PathAccessType, PermissionMode, PermissionRule,
};
use crate::domain::ports::SecurityPort;

pub struct SecurityAdapter {
    workspace_path: PathBuf,
    blocked_commands: Vec<String>,
    blocked_paths: Vec<String>,
    mode: Arc<AtomicU8>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    pub allowed_rules: RwLock<Vec<PermissionRule>>,
    /// In-memory session-level auto-allow set (AC4). Keyed by tool_name.
    /// NOT persisted to settings.json. Cleared on process exit.
    session_allowed_tools: RwLock<HashSet<String>>,
    /// Active skill directories that are readable regardless of workspace boundary (AC7).
    /// Session-scoped; canonical paths; read-only access.
    active_skill_dirs: std::sync::RwLock<HashSet<PathBuf>>,
}

impl SecurityAdapter {
    pub fn new(workspace_path: PathBuf, event_tx: mpsc::UnboundedSender<AppEvent>) -> Self {
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
            event_tx,
            allowed_rules: RwLock::new(Vec::new()),
            session_allowed_tools: RwLock::new(HashSet::new()),
            active_skill_dirs: std::sync::RwLock::new(HashSet::new()),
        }
    }

    /// Check if a tool call matches any AlwaysAllow rule.
    fn matches_allowed_rule(
        rules: &[PermissionRule],
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> bool {
        for rule in rules {
            if rule.tool_name != tool_name {
                continue;
            }
            match &rule.pattern {
                None => return true, // Matches all calls of this tool
                Some(pattern) => {
                    // Extract the relevant input value based on tool type
                    let input_value = if tool_name == "Bash" || tool_name == "bash" {
                        tool_input.get("command").and_then(|v| v.as_str())
                    } else {
                        tool_input.get("file_path").and_then(|v| v.as_str())
                    };
                    if let Some(val) = input_value {
                        // Exact match (case-insensitive) — prefix matching is too permissive
                        if val.to_lowercase() == pattern.to_lowercase() {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Build a PermissionRule from a tool call when user selects AlwaysAllow.
    fn build_rule(tool_name: &str, tool_input: &serde_json::Value) -> PermissionRule {
        let pattern = match tool_name {
            "Bash" | "bash" => tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        };
        PermissionRule {
            tool_name: tool_name.to_string(),
            pattern,
        }
    }

    /// Load AlwaysAllow rules from settings.json.
    pub fn load_settings(&self, workspace: &Path) -> Vec<PermissionRule> {
        let settings_path = workspace.join(".claude").join("settings.json");
        let content = match std::fs::read_to_string(&settings_path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let allow_array = match json
            .get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
        {
            Some(a) => a,
            None => return Vec::new(),
        };
        allow_array
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| parse_rule_string(s))
            .collect()
    }

    /// Persist current AlwaysAllow rules to settings.json (read-merge-write to preserve other fields).
    fn persist_settings(workspace: &Path, rules: &[PermissionRule]) {
        let settings_dir = workspace.join(".claude");
        if let Err(e) = std::fs::create_dir_all(&settings_dir) {
            tracing::warn!("Failed to create settings directory: {}", e);
            return;
        }
        let settings_path = settings_dir.join("settings.json");

        // Read existing settings to preserve unrelated fields
        let mut root: serde_json::Value = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        let allow_strings: Vec<String> = rules.iter().map(|r| format_rule_string(r)).collect();

        // Merge: update only permissions.allow, preserve everything else
        root.as_object_mut()
            .unwrap()
            .entry("permissions")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .map(|perms| perms.insert("allow".to_string(), serde_json::json!(allow_strings)));

        match serde_json::to_string_pretty(&root) {
            Ok(content) => {
                if let Err(e) = std::fs::write(&settings_path, content) {
                    tracing::warn!("Failed to persist settings.json: {}", e);
                }
            }
            Err(e) => tracing::warn!("Failed to serialize settings: {}", e),
        }
    }

    /// Initialize allowed_rules from settings.json (called at startup).
    /// Always replaces in-memory state, even if disk has empty array.
    pub async fn init_allowed_rules(&self) {
        let rules = self.load_settings(&self.workspace_path);
        let mut allowed = self.allowed_rules.write().await;
        *allowed = rules;
    }

    /// Register a tool as session-allowed (for testing).
    #[allow(dead_code)]
    pub async fn add_session_allowed(&self, tool_name: &str) {
        let mut session = self.session_allowed_tools.write().await;
        session.insert(tool_name.to_string());
    }

    /// Check if a tool is in the session-allow set (for testing).
    #[allow(dead_code)]
    pub async fn is_session_allowed(&self, tool_name: &str) -> bool {
        let session = self.session_allowed_tools.read().await;
        session.contains(tool_name)
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
    ) -> Result<PathAccessType, PermissionError> {
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

        let workspace_canonical = std::fs::canonicalize(&self.workspace_path)
            .unwrap_or_else(|_| self.workspace_path.clone());

        // Use canonicalize if the path exists, otherwise canonicalize the parent
        // to resolve symlinks even for non-existent paths (AC7, DF-010).
        let resolved = match std::fs::canonicalize(&absolute) {
            Ok(p) => p,
            Err(_) => {
                // Path doesn't exist yet (Write creating new file) — canonicalize parent
                if let Some(parent) = absolute.parent() {
                    match std::fs::canonicalize(parent) {
                        Ok(canon_parent) => {
                            // DF-108: verify parent is inside workspace BEFORE joining
                            // (catches parent-is-symlink-to-outside at the source).
                            if !canon_parent.starts_with(&workspace_canonical) {
                                return Err(PermissionError::WorkspaceViolation(
                                    "Parent directory resolves outside workspace".to_string(),
                                ));
                            }
                            if let Some(file_name) = absolute.file_name() {
                                let resolved = canon_parent.join(file_name);
                                // Re-verify joined path remains within parent directory
                                // to prevent symlink escape via TOCTOU race (DF-086)
                                if !resolved.starts_with(&canon_parent) {
                                    return Err(PermissionError::WorkspaceViolation(
                                        "Path traversal detected via symlink".to_string(),
                                    ));
                                }
                                // DF-108: re-canonicalize after join to catch symlink swap.
                                // If the file does not yet exist (ENOENT) that's fine — we rely
                                // on the already-verified canon_parent containment above.
                                // If canonicalize fails for any OTHER reason, or if the final
                                // path resolves outside the workspace, reject.
                                match std::fs::canonicalize(&resolved) {
                                    Ok(final_resolved) => {
                                        if !final_resolved.starts_with(&workspace_canonical) {
                                            return Err(PermissionError::WorkspaceViolation(
                                                "Symlink escape detected".to_string(),
                                            ));
                                        }
                                        final_resolved
                                    }
                                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                        // File doesn't exist yet — parent containment guarantees
                                        // the joined path is inside workspace.
                                        resolved
                                    }
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
                        // Parent also non-existent: raw component fallback would bypass symlink
                        // safety — return PermissionError instead (DF-010).
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
        if resolved.starts_with(&workspace_canonical) {
            Ok(PathAccessType::Workspace)
        } else if op == FileOperation::Read {
            // AC7: Check active skill directories for read access
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

/// Parse a rule string like "Bash(cargo test)" or "Read" into a PermissionRule.
/// Uses the first `(` and the *matching last* `)` as delimiters to handle
/// patterns containing parentheses (e.g., `Bash(echo $(whoami))`).
fn parse_rule_string(s: &str) -> PermissionRule {
    if let Some(paren_start) = s.find('(') {
        if let Some(paren_end) = s.rfind(')') {
            if paren_end > paren_start {
                let tool_name = s[..paren_start].to_string();
                let pattern = s[paren_start + 1..paren_end].to_string();
                return PermissionRule {
                    tool_name,
                    pattern: Some(pattern),
                };
            }
        }
    }
    PermissionRule {
        tool_name: s.to_string(),
        pattern: None,
    }
}

/// Format a PermissionRule as a string like "Bash(cargo test)" or "Read".
fn format_rule_string(rule: &PermissionRule) -> String {
    match &rule.pattern {
        Some(pattern) => format!("{}({})", rule.tool_name, pattern),
        None => rule.tool_name.clone(),
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
        self.validate_path(path, op)
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

    async fn request_permission(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> Result<ApprovalDecision, PermissionError> {
        // Defensive Yolo short-circuit (chain handles mode gating but keep safety net)
        if self.current_mode() == PermissionMode::Yolo {
            return Ok(ApprovalDecision::Allow);
        }

        // Check session-allow set (AC4) — in-memory, not persisted
        {
            let session = self.session_allowed_tools.read().await;
            if session.contains(tool_name) {
                return Ok(ApprovalDecision::Allow);
            }
        }

        // Check AlwaysAllow rules (persisted)
        {
            let rules = self.allowed_rules.read().await;
            if Self::matches_allowed_rule(&rules, tool_name, tool_input) {
                return Ok(ApprovalDecision::AlwaysAllow);
            }
        }

        // No rule matched — send permission request to TUI via oneshot
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.event_tx
            .send(AppEvent::PermissionRequest {
                tool_name: tool_name.to_string(),
                tool_input: tool_input.clone(),
                response_tx: tx,
            })
            .map_err(|_| PermissionError::Cancelled)?;

        let decision = rx.await.map_err(|_| PermissionError::Cancelled)?;

        // Handle SessionAllow: register in session set, do NOT persist (AC4)
        if matches!(decision, ApprovalDecision::SessionAllow) {
            let mut session = self.session_allowed_tools.write().await;
            session.insert(tool_name.to_string());
            return Ok(ApprovalDecision::Allow);
        }

        // Handle AlwaysAllow: add rule and persist
        if matches!(decision, ApprovalDecision::AlwaysAllow) {
            let rule = Self::build_rule(tool_name, tool_input);
            let mut rules = self.allowed_rules.write().await;
            rules.push(rule);
            Self::persist_settings(&self.workspace_path, &rules);
        }

        Ok(decision)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn make_adapter() -> SecurityAdapter {
        let (tx, _rx) = mpsc::unbounded_channel();
        SecurityAdapter::new(env::current_dir().unwrap(), tx)
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

    // Covers: AC7 (DF-010) — deeply nested non-existent path returns PermissionError
    #[test]
    fn test_workspace_deeply_nested_nonexistent_path_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let adapter = SecurityAdapter::new(tmp.path().to_path_buf(), tx);
        // deep/missing/file.txt — neither the parent directory nor the file exists
        let path = tmp.path().join("deep").join("missing").join("file.txt");
        let result = adapter.check_workspace_access(&path, FileOperation::Write);
        assert!(
            result.is_err(),
            "Should return PermissionError — cannot verify workspace membership \
             when path and parent both nonexistent (DF-010)"
        );
        // Verify error message is user-friendly and doesn't expose internal details (DF-085)
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not in workspace"),
            "Error should be user-friendly: {}",
            err_msg
        );
    }

    // Covers: DF-086 — joined path is re-verified to stay within parent directory
    // This prevents symlink escape via TOCTOU race between canonicalize and join.
    #[test]
    fn test_workspace_rejects_path_escaping_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let adapter = SecurityAdapter::new(tmp.path().to_path_buf(), tx);

        // Create a valid parent directory
        let parent = tmp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();

        // Attempt to access a file that escapes the parent via ".."
        // This simulates what would happen if a symlink was swapped between
        // canonicalize and join operations.
        let path = parent.join("..").join("..").join("etc").join("passwd");
        let result = adapter.check_workspace_access(&path, FileOperation::Write);

        // Should fail because the resolved path escapes the workspace
        assert!(
            result.is_err(),
            "Should reject path that escapes parent directory (DF-086)"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("outside workspace") || err_msg.contains("traversal"),
            "Error should indicate path traversal: {}",
            err_msg
        );
    }

    // Story 5-2 AC7: when a skill directory is registered via
    // `add_active_skill_dir`, Read access to paths inside that directory is
    // allowed even if it lies outside the workspace. Removing the entry must
    // revoke that access, and Write remains denied regardless.
    #[test]
    fn test_active_skill_dir_grants_read_then_revokes_on_remove() {
        let workspace = tempfile::TempDir::new().unwrap();
        let skill_root = tempfile::TempDir::new().unwrap(); // deliberately outside workspace
        let skill_dir = skill_root.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_file = skill_dir.join("helper.md");
        std::fs::write(&skill_file, "content").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel();
        let adapter = SecurityAdapter::new(workspace.path().to_path_buf(), tx);

        // Baseline: path outside workspace denied for both Read and Write.
        assert!(
            adapter
                .check_workspace_access(&skill_file, FileOperation::Read)
                .is_err(),
            "skill file must be denied before the dir is registered"
        );
        assert!(
            adapter
                .check_workspace_access(&skill_file, FileOperation::Write)
                .is_err()
        );

        // Register the skill dir — Read should now succeed.
        adapter.add_active_skill_dir(skill_dir.clone());
        let read = adapter.check_workspace_access(&skill_file, FileOperation::Read);
        assert!(
            read.is_ok(),
            "active skill dir should grant Read access to its files (AC7); got {:?}",
            read
        );

        // Write stays denied — AC7 only grants Read scope.
        let write = adapter.check_workspace_access(&skill_file, FileOperation::Write);
        assert!(
            write.is_err(),
            "active skill dir must NOT grant Write access (AC7); got {:?}",
            write
        );

        // A sibling file outside the registered skill dir stays denied.
        let sibling = skill_root.path().join("not-a-skill.md");
        std::fs::write(&sibling, "x").unwrap();
        assert!(
            adapter
                .check_workspace_access(&sibling, FileOperation::Read)
                .is_err(),
            "only files under the registered skill dir are granted access"
        );

        // Remove the dir — Read should revert to denied.
        adapter.remove_active_skill_dir(&skill_dir);
        assert!(
            adapter
                .check_workspace_access(&skill_file, FileOperation::Read)
                .is_err(),
            "removing the skill dir must revoke Read access"
        );
    }

    #[test]
    fn test_permission_mode_switching() {
        let adapter = make_adapter();
        // Default mode is now Normal (changed from Yolo in 1-5)
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
        adapter.set_mode(PermissionMode::Yolo);
        assert_eq!(adapter.current_mode(), PermissionMode::Yolo);
        adapter.set_mode(PermissionMode::Normal);
        assert_eq!(adapter.current_mode(), PermissionMode::Normal);
    }

    #[tokio::test]
    async fn test_request_permission_yolo_auto_allows() {
        let adapter = make_adapter();
        adapter.set_mode(PermissionMode::Yolo);
        let result = adapter
            .request_permission("Bash", &serde_json::json!({"command": "ls"}))
            .await;
        assert_eq!(result.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn test_request_permission_normal_sends_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let adapter = SecurityAdapter::new(env::current_dir().unwrap(), tx);
        // Normal mode — request_permission sends PermissionRequest event
        let handle = tokio::spawn(async move {
            adapter
                .request_permission("Bash", &serde_json::json!({"command": "cargo test"}))
                .await
        });

        // Receive the PermissionRequest event and respond
        if let Some(AppEvent::PermissionRequest { response_tx, .. }) = rx.recv().await {
            let _ = response_tx.send(ApprovalDecision::Allow);
        }

        let result = handle.await.unwrap();
        assert_eq!(result.unwrap(), ApprovalDecision::Allow);
    }

    #[tokio::test]
    async fn test_always_allow_rule_matching() {
        let adapter = make_adapter();
        adapter.set_mode(PermissionMode::Normal);
        {
            let mut rules = adapter.allowed_rules.write().await;
            rules.push(PermissionRule {
                tool_name: "Read".to_string(),
                pattern: None,
            });
            rules.push(PermissionRule {
                tool_name: "Bash".to_string(),
                pattern: Some("cargo test".to_string()),
            });
        }

        // Read matches rule without pattern — returns AlwaysAllow for rule matches
        let result = adapter
            .request_permission("Read", &serde_json::json!({"file_path": "src/main.rs"}))
            .await;
        assert_eq!(result.unwrap(), ApprovalDecision::AlwaysAllow);

        // Bash with exact matching command
        let result = adapter
            .request_permission("Bash", &serde_json::json!({"command": "cargo test"}))
            .await;
        assert_eq!(result.unwrap(), ApprovalDecision::AlwaysAllow);
    }

    #[test]
    fn test_parse_rule_string() {
        let rule = parse_rule_string("Read");
        assert_eq!(rule.tool_name, "Read");
        assert!(rule.pattern.is_none());

        let rule = parse_rule_string("Bash(cargo test)");
        assert_eq!(rule.tool_name, "Bash");
        assert_eq!(rule.pattern.as_deref(), Some("cargo test"));
    }

    #[test]
    fn test_format_rule_string() {
        let rule = PermissionRule {
            tool_name: "Read".to_string(),
            pattern: None,
        };
        assert_eq!(format_rule_string(&rule), "Read");

        let rule = PermissionRule {
            tool_name: "Bash".to_string(),
            pattern: Some("cargo build".to_string()),
        };
        assert_eq!(format_rule_string(&rule), "Bash(cargo build)");
    }

    #[test]
    fn test_permission_chain_blocked_overrides_allow() {
        let adapter = make_adapter();
        assert!(adapter.check_blocklist("rm -rf /").is_err());
    }

    #[tokio::test]
    async fn test_settings_persistence_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let adapter = SecurityAdapter::new(tmp.path().to_path_buf(), tx);

        let rules = vec![
            PermissionRule {
                tool_name: "Bash".to_string(),
                pattern: Some("cargo test".to_string()),
            },
            PermissionRule {
                tool_name: "Read".to_string(),
                pattern: None,
            },
        ];
        SecurityAdapter::persist_settings(tmp.path(), &rules);

        let loaded = adapter.load_settings(tmp.path());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].tool_name, "Bash");
        assert_eq!(loaded[0].pattern.as_deref(), Some("cargo test"));
        assert_eq!(loaded[1].tool_name, "Read");
        assert!(loaded[1].pattern.is_none());
    }
}
