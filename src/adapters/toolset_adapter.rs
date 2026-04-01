//! Concrete ToolSetPort adapter.
//! Implements Bash, Read, Write tool execution with file snapshots and write serialization.
//! Follows the same patterns as rustycode's ToolExecutor but implemented directly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::domain::errors::ToolError;
use crate::domain::models::{ToolDefinition, ToolResult};
use crate::domain::ports::ToolSetPort;

/// ToolSetPort implementation with Bash, Read, Write tools.
pub struct ToolSetAdapter {
    workspace_path: PathBuf,
    write_locks: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
    session_id: String,
    /// Monotonically increasing counter for unique snapshot message indices.
    message_counter: AtomicUsize,
}

impl ToolSetAdapter {
    pub fn new(workspace_path: PathBuf, session_id: String) -> Self {
        Self {
            workspace_path,
            write_locks: Arc::new(Mutex::new(HashMap::new())),
            session_id,
            message_counter: AtomicUsize::new(0),
        }
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

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let content = if stderr.is_empty() {
            stdout.to_string()
        } else if stdout.is_empty() {
            stderr.to_string()
        } else {
            format!("{}{}", stdout, stderr)
        };

        Ok(ToolResult {
            tool_use_id: String::new(), // Set by caller
            content,
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

        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            ToolError::ExecutionFailed(format!("Failed to read '{}': {}", file_path, e))
        })?;
        let content = String::from_utf8_lossy(&bytes).into_owned();

        // Apply offset and limit (line-based, cat -n style)
        let lines: Vec<&str> = content.lines().collect();
        let selected: Vec<String> = lines
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit)
            .map(|(i, line)| format!("{}\t{}", i + 1, line))
            .collect();

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
        message_index: usize,
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

        // Take snapshot before writing (AC10)
        self.take_snapshot(&path, message_index).await;

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

        let byte_count = new_content.len();
        Ok(ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: format!("Successfully wrote {} bytes to {}", byte_count, file_path),
            is_error: false,
        })
    }

    /// Take a file snapshot before modification (AC10).
    async fn take_snapshot(&self, path: &PathBuf, message_index: usize) {
        let snapshot_dir = self
            .workspace_path
            .join(".claude")
            .join("sessions")
            .join(&self.session_id)
            .join("snapshots");

        // Create snapshot directory lazily
        if let Err(e) = tokio::fs::create_dir_all(&snapshot_dir).await {
            tracing::warn!("Failed to create snapshot directory: {}", e);
            return;
        }

        let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let path_str = canonical_path.to_string_lossy();

        // Compute path hash for filename
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let short_hash = &hash[..16.min(hash.len())];

        let snapshot_file = snapshot_dir.join(format!("{}_{}.json", short_hash, message_index));

        // Read original content (if file exists)
        let (original_content, original_hash) = match tokio::fs::read_to_string(path).await {
            Ok(content) => {
                let mut hasher = Sha256::new();
                hasher.update(content.as_bytes());
                let hash = format!("{:x}", hasher.finalize());
                (Some(content), Some(hash))
            }
            Err(_) => (None, None),
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let snapshot = serde_json::json!({
            "path": path_str,
            "original_hash": original_hash,
            "original_content": original_content,
            "message_index": message_index,
            "timestamp_ms": now_ms,
        });

        if let Err(e) = tokio::fs::write(
            &snapshot_file,
            serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
        )
        .await
        {
            tracing::warn!("Failed to write snapshot: {}", e);
        }
    }
}

impl std::fmt::Debug for ToolSetAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSetAdapter")
            .field("workspace_path", &self.workspace_path)
            .field("session_id", &self.session_id)
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
            "Write" | "write" => {
                let idx = self.message_counter.fetch_add(1, Ordering::Relaxed);
                self.execute_write(&input, "", idx).await
            }
            _ => Err(ToolError::NotFound(tool_name.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_adapter(dir: &std::path::Path) -> ToolSetAdapter {
        ToolSetAdapter::new(dir.to_path_buf(), "test-session".to_string())
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

    #[tokio::test]
    async fn test_write_snapshot_created() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("existing.txt");
        std::fs::write(&file, "original content").unwrap();

        let adapter = make_adapter(tmp.path());
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

        let snapshot_dir = tmp
            .path()
            .join(".claude")
            .join("sessions")
            .join("test-session")
            .join("snapshots");
        assert!(snapshot_dir.exists());
        let entries: Vec<_> = std::fs::read_dir(&snapshot_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);

        let snapshot_content =
            std::fs::read_to_string(entries[0].as_ref().unwrap().path()).unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&snapshot_content).unwrap();
        assert_eq!(
            snapshot["original_content"].as_str().unwrap(),
            "original content"
        );
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
