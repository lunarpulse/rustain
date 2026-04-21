//! Concrete ToolSetPort adapter.
//! Implements Bash, Read, Write tool execution with file snapshots and write serialization.
//! Follows the same patterns as rustycode's ToolExecutor but implemented directly.
//!
//! Story 4-3b: The old workspace-rooted `take_snapshot` mechanism (storing to
//! `.claude/sessions/{global_session_id}/snapshots/`) has been REPLACED by the
//! `StoragePort::snapshot_file()` protocol which stores snapshots co-located with
//! their conversation at `{sessions_dir}/{conversation_id}/snapshots/`.
//! The old path leaked across conversations and was not cleaned up by `delete_conversation()`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::adapters::skill_activation::SkillActivator;
use crate::domain::errors::ToolError;
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{ToolDefinition, ToolResult};
use crate::domain::ports::{StoragePort, ToolSetPort};

/// Active checkpoint context for file snapshotting within a turn.
#[derive(Clone)]
struct ToolExecutionContext {
    conversation_id: String,
    checkpoint: CheckpointId,
    /// Maximum activation depth across skills active in this conversation —
    /// used as `caller_depth` when the model invokes the `activate_skill` tool
    /// so that `MAX_SKILL_ACTIVATION_DEPTH` is enforced across chained activations.
    activation_depth: u8,
}

/// ToolSetPort implementation with Bash, Read, Write tools.
pub struct ToolSetAdapter {
    workspace_path: PathBuf,
    write_locks: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
    storage: Arc<dyn StoragePort>,
    /// Active checkpoint context for the current tool-executing turn.
    /// Set by `set_execution_context` before any tools run; cleared between turns.
    current_context: Mutex<Option<ToolExecutionContext>>,
    /// Optional skill activator for `activate_skill` tool execution.
    #[allow(dead_code)]
    activator: Option<Arc<SkillActivator>>,
}

impl ToolSetAdapter {
    pub fn new(workspace_path: PathBuf, storage: Arc<dyn StoragePort>) -> Self {
        Self {
            workspace_path,
            write_locks: Arc::new(Mutex::new(HashMap::new())),
            storage,
            current_context: Mutex::new(None),
            activator: None,
        }
    }

    #[allow(dead_code)]
    pub fn set_activator(&mut self, activator: Arc<SkillActivator>) {
        self.activator = Some(activator);
    }

    async fn execute_bash(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'command' parameter".into()))?;

        let timeout_ms = input
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000)
            .min(600_000); // Cap at 10 minutes

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            tokio::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(&self.workspace_path)
                .output(),
        )
        .await
        .map_err(|_| ToolError::Timeout)?
        .map_err(|e| ToolError::ExecutionFailed(format!("Failed to spawn bash: {}", e)))?;

        let mut result = String::new();
        if !output.stdout.is_empty() {
            result.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        if result.is_empty() {
            result = format!("Command exited with status {}", output.status);
        }

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: result,
            is_error: !output.status.success(),
        })
    }

    async fn execute_read(&self, input: &serde_json::Value) -> Result<ToolResult, ToolError> {
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'file_path' parameter".into()))?;

        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;

        let path = if std::path::Path::new(file_path).is_absolute() {
            PathBuf::from(file_path)
        } else {
            self.workspace_path.join(file_path)
        };

        // Read raw bytes to preserve binary content; lossy-decode for line iteration.
        // This mirrors rustycode's Read tool contract — binary files return a
        // best-effort textual view rather than failing outright.
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read '{}': {}", file_path, e))
        })?;
        let content = String::from_utf8_lossy(&bytes).into_owned();

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let selected: Vec<String> = lines
            .into_iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, l)| format!("{}\t{}", i + 1, l))
            .collect();

        if selected.is_empty() {
            return Ok(ToolResult {
                tool_use_id: String::new(),
                content: format!(
                    "File '{}' has {} lines total; offset {} is past end",
                    file_path, total, offset
                ),
                is_error: false,
            });
        }

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: selected.join("\n"),
            is_error: false,
        })
    }

    async fn execute_write(
        &self,
        input: &serde_json::Value,
        tool_use_id: &str,
    ) -> Result<ToolResult, ToolError> {
        let file_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'file_path' parameter".into()))?;

        let new_content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'content' parameter".into()))?;

        let path = if std::path::Path::new(file_path).is_absolute() {
            PathBuf::from(file_path)
        } else {
            self.workspace_path.join(file_path)
        };

        // Acquire per-path write lock (NFR26)
        let per_path_lock = {
            let mut locks = self.write_locks.lock().await;
            locks
                .entry(path.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _write_guard = per_path_lock.lock().await;

        // Take snapshot before writing via StoragePort (Story 4-3b).
        // Also capture original_hash for DF-107 TOCTOU re-check just before write.
        let snapshot_ctx = self.current_context.lock().await.clone();
        let original_hash_for_toctou: Option<String> = if let Some(ref ctx) = snapshot_ctx {
            let original = match tokio::fs::read(&path).await {
                Ok(data) => data,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(e) => {
                    tracing::warn!("snapshot pre-read failed for {}: {}", path.display(), e);
                    vec![]
                }
            };

            // Compute hash of original content for TOCTOU check (DF-107, B2).
            // Empty vec from NotFound = new file creation, no TOCTOU concern.
            let hash = if !original.is_empty() {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(&original);
                Some(format!("sha256:{:x}", h.finalize()))
            } else {
                None
            };

            if let Err(e) = self
                .storage
                .snapshot_file(&ctx.conversation_id, ctx.checkpoint, &path, &original)
                .await
            {
                tracing::warn!("snapshot_file failed for {}: {}", path.display(), e);
            }
            hash
        } else {
            tracing::warn!(
                "Write tool executed without an active checkpoint context — no snapshot taken for {}",
                path.display()
            );
            None
        };

        // DF-107 (AC2): TOCTOU re-hash — verify the file has not been externally
        // modified between the snapshot read and this write. Re-read and re-hash
        // the file immediately before writing. If the hash diverges, report a
        // Conflict rather than silently overwriting an external change.
        // Re-hashing (not advisory flock) is chosen: portable, no OS-level lock
        // inheritance issues, and sufficient for single-machine tool execution.
        // Documented here per AC2: rationale for mechanism selection.
        if let Some(ref expected_hash) = original_hash_for_toctou {
            match tokio::fs::read(&path).await {
                Ok(current_content) if !current_content.is_empty() => {
                    use sha2::{Digest, Sha256};
                    let mut h = Sha256::new();
                    h.update(&current_content);
                    let actual_hash = format!("sha256:{:x}", h.finalize());
                    if actual_hash != *expected_hash {
                        return Ok(ToolResult {
                            tool_use_id: tool_use_id.to_string(),
                            content: format!(
                                "TOCTOU conflict: '{}' was modified between snapshot and write.\n\
                                 expected_hash: {}\n\
                                 actual_hash: {}\n\
                                 Rewind protection is intact — please retry or resolve the conflict.",
                                file_path, expected_hash, actual_hash
                            ),
                            is_error: true,
                        });
                    }
                }
                Ok(_) => {
                    // File now empty — was non-empty when we snapshotted. Conflict.
                    return Ok(ToolResult {
                        tool_use_id: tool_use_id.to_string(),
                        content: format!(
                            "TOCTOU conflict: '{}' was truncated or emptied between snapshot and write.\n\
                             expected_hash: {}\n\
                             Rewind protection is intact — please retry or resolve the conflict.",
                            file_path, expected_hash
                        ),
                        is_error: true,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File deleted between snapshot and write — that's a conflict.
                    return Ok(ToolResult {
                        tool_use_id: tool_use_id.to_string(),
                        content: format!(
                            "TOCTOU conflict: '{}' was deleted between snapshot and write.\n\
                             expected_hash: {}\n\
                             Rewind protection is intact — please retry or resolve the conflict.",
                            file_path, expected_hash
                        ),
                        is_error: true,
                    });
                }
                Err(e) => {
                    tracing::warn!("TOCTOU re-check read failed for {}: {}", path.display(), e);
                    // Proceed with write — cannot verify, but don't block the tool.
                }
            }
        }

        // Create parent directories
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to create directories: {}", e))
            })?;
        }

        // Write file
        tokio::fs::write(&path, new_content).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to write '{}': {}", file_path, e))
        })?;

        // DF-111 (AC5, schema v3): record post-write hash so revert can distinguish
        // "tool-modified" (→ Restore) from "externally-modified" (→ Conflict).
        if let Some(ref ctx) = snapshot_ctx {
            if let Err(e) = self
                .storage
                .finalize_snapshot(
                    &ctx.conversation_id,
                    ctx.checkpoint,
                    &path,
                    new_content.as_bytes(),
                )
                .await
            {
                // Non-fatal: degrades to v2 semantics (current != original → Restore).
                tracing::debug!("finalize_snapshot failed for {}: {}", path.display(), e);
            }
        }

        let byte_count = new_content.len();
        Ok(ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: format!("Successfully wrote {} bytes to {}", byte_count, file_path),
            is_error: false,
        })
    }
}

impl std::fmt::Debug for ToolSetAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSetAdapter")
            .field("workspace_path", &self.workspace_path)
            .finish()
    }
}

#[async_trait]
impl ToolSetPort for ToolSetAdapter {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "Bash".to_string(),
                description: "Execute a bash command".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The bash command to execute"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Timeout in milliseconds (default 120000)"
                        }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "Read".to_string(),
                description: "Read a file from the filesystem".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The path to the file to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Line offset to start reading from (default 0)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to read (default 2000)"
                        }
                    },
                    "required": ["file_path"]
                }),
            },
            ToolDefinition {
                name: "Write".to_string(),
                description: "Write content to a file".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "The path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write to the file"
                        }
                    },
                    "required": ["file_path", "content"]
                }),
            },
            ToolDefinition {
                name: "activate_skill".to_string(),
                description: "Activate an Agent Skill to gain its procedural instructions and tool restrictions. Arg: name of the skill to activate (must match a discovered skill).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name (exact match, case-sensitive)"
                        },
                        "arguments": {
                            "type": "string",
                            "description": "Optional trailing arguments passed to the skill"
                        }
                    },
                    "required": ["name"]
                }),
            },
        ]
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        match tool_name {
            "Bash" | "bash" => self.execute_bash(&input).await,
            "Read" | "read" => self.execute_read(&input).await,
            "Write" | "write" => self.execute_write(&input, "").await,
            "activate_skill" => self.execute_activate_skill(&input).await,
            _ => Err(ToolError::NotFound(tool_name.to_string())),
        }
    }

    /// Set the active checkpoint context for file snapshotting (Story 4-3b, AC2).
    async fn set_execution_context(
        &self,
        conversation_id: String,
        checkpoint: CheckpointId,
        activation_depth: u8,
    ) {
        *self.current_context.lock().await = Some(ToolExecutionContext {
            conversation_id,
            checkpoint,
            activation_depth,
        });
    }
}

impl ToolSetAdapter {
    async fn execute_activate_skill(
        &self,
        input: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'name' parameter".into()))?;

        let arguments = input
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let activator = self
            .activator
            .as_ref()
            .ok_or_else(|| ToolError::ExecutionFailed("Skill activator not configured".into()))?;

        let (conv_id, caller_depth) = {
            let context = self.current_context.lock().await;
            context
                .as_ref()
                .map(|c| (c.conversation_id.clone(), c.activation_depth))
                .ok_or_else(|| {
                    ToolError::ExecutionFailed(
                        "Skill activation requires an active conversation context".into(),
                    )
                })?
        };

        let result = activator
            .activate_by_name(name, arguments, &conv_id, caller_depth)
            .await;

        match result {
            Ok(crate::domain::models::SkillActivationOutcome::Activated(_)) => Ok(ToolResult {
                tool_use_id: String::new(),
                content: format!("Skill '{}' activated.", name),
                is_error: false,
            }),
            Ok(crate::domain::models::SkillActivationOutcome::TrustDeclined(n)) => {
                // Decision 4: decline is a user choice, not an error. Surface it
                // to the model as an informational tool result so the model can
                // adjust its plan without treating it as a failure.
                Ok(ToolResult {
                    tool_use_id: String::new(),
                    content: format!("Skill '{}' not trusted — activation declined.", n),
                    is_error: false,
                })
            }
            Err(crate::domain::models::SkillActivationError::NotFound(n)) => {
                let names = activator.discovered_skill_names().await;
                Ok(ToolResult {
                    tool_use_id: String::new(),
                    content: format!(
                        "Skill not found: {}. Discovered skills: [{}]",
                        n,
                        names.join(", ")
                    ),
                    is_error: true,
                })
            }
            Err(e) => Ok(ToolResult {
                tool_use_id: String::new(),
                content: e.to_string(),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;

    fn make_adapter(dir: &std::path::Path) -> ToolSetAdapter {
        let sessions_dir = dir.join(".claude").join("sessions");
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::new(sessions_dir));
        ToolSetAdapter::new(dir.to_path_buf(), storage)
    }

    #[tokio::test]
    async fn test_bash_echo() {
        let dir = std::env::current_dir().unwrap();
        let adapter = make_adapter(&dir);
        let result = adapter
            .execute("Bash", serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert_eq!(result.content.trim(), "hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_read_tempfile() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "line1\nline2\nline3").unwrap();

        let adapter = make_adapter(tmp.path());
        let result = adapter
            .execute(
                "Read",
                serde_json::json!({"file_path": file.to_str().unwrap()}),
            )
            .await
            .unwrap();
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line2"));
        assert!(!result.is_error);
    }

    /// Regression: Story 4-3b review P1 — Read tool must not fail on binary files.
    /// Prior implementation used `read_to_string` which errored on any non-UTF8 byte.
    /// Current implementation reads raw bytes and lossy-decodes, matching rustycode's contract.
    #[tokio::test]
    async fn test_read_tool_preserves_binary_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("binary.bin");
        // Write non-UTF8 bytes (0xFF 0xFE is invalid UTF-8 start, 0x00 is null, 0x41 is 'A').
        std::fs::write(&file, [0xFFu8, 0xFE, 0x00, 0x41, b'\n', 0xC3, 0x28]).unwrap();

        let adapter = make_adapter(tmp.path());
        let result = adapter
            .execute(
                "Read",
                serde_json::json!({"file_path": file.to_str().unwrap()}),
            )
            .await
            .expect("Read must not fail on binary content");

        assert!(
            !result.is_error,
            "binary read should succeed via lossy decode"
        );
        // Replacement char U+FFFD should appear for invalid sequences, 'A' should survive.
        assert!(
            result.content.contains('A'),
            "ASCII content must survive lossy decode"
        );
        assert!(
            result.content.contains('\u{FFFD}'),
            "invalid UTF-8 should map to replacement chars, not an error"
        );
    }

    #[tokio::test]
    async fn test_write_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("output.txt");

        let adapter = make_adapter(tmp.path());
        let result = adapter
            .execute(
                "Write",
                serde_json::json!({
                    "file_path": file.to_str().unwrap(),
                    "content": "hello world"
                }),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("11 bytes"));

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "hello world");
    }

    /// Verifies that when a checkpoint context is active, a snapshot is created
    /// at the per-conversation path before the Write tool modifies the file.
    #[tokio::test]
    async fn test_write_snapshot_created() {
        use crate::domain::models::conversation::generate_conversation_id;

        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join(".claude").join("sessions");
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::new(sessions_dir.clone()));
        let adapter = ToolSetAdapter::new(tmp.path().to_path_buf(), Arc::clone(&storage));

        // Create a minimal conversation so that create_checkpoint has somewhere to write.
        let conv_id = generate_conversation_id();
        let conv = crate::domain::models::Conversation {
            id: conv_id.clone(),
            title: "test".to_string(),
            messages: vec![crate::domain::models::ChatMessage {
                id: "msg-1".to_string(),
                role: crate::domain::models::MessageRole::User,
                content: "hello".to_string(),
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: 0,
                token_count: None,
                stop_reason: None,
                images: vec![],
            }],
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            fork_source: None,
        };
        storage.save_conversation(&conv).await.unwrap();

        // Create a checkpoint and set context.
        let cp = storage.create_checkpoint(&conv_id).await.unwrap();
        adapter.set_execution_context(conv_id.clone(), cp, 0).await;

        // Write an existing file — should snapshot first.
        let file = tmp.path().join("existing.txt");
        std::fs::write(&file, "original content").unwrap();

        adapter
            .execute(
                "Write",
                serde_json::json!({
                    "file_path": file.to_str().unwrap(),
                    "content": "new content"
                }),
            )
            .await
            .unwrap();

        // The snapshot should be in {sessions_dir}/{conv_id}/snapshots/
        let snapshot_dir = sessions_dir.join(&conv_id).join("snapshots");
        assert!(
            snapshot_dir.exists(),
            "snapshot dir should exist at {:?}",
            snapshot_dir
        );
        let entries: Vec<_> = std::fs::read_dir(&snapshot_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly 1 snapshot file");

        let snapshot_content = std::fs::read_to_string(entries[0].path()).unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&snapshot_content).unwrap();

        // Verify envelope structure (schema v3 — DF-111 expected_current_hash field).
        assert_eq!(snapshot["schema_version"].as_u64().unwrap(), 3);
        assert_eq!(snapshot["conversation_id"].as_str().unwrap(), conv_id);
        assert!(
            snapshot["file_existed"].as_bool().unwrap(),
            "existing-file snapshot must set file_existed=true"
        );
        assert!(
            snapshot["original_hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );

        // Verify original content is stored (base64).
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(snapshot["original_content_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"original content");
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let adapter = make_adapter(tmp.path());
        let result = adapter.execute("UnknownTool", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_invalid_input_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let adapter = make_adapter(tmp.path());
        let result = adapter.execute("Bash", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
