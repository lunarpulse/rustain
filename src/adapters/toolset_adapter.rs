//! Concrete ToolSetPort adapter.
//! Implements Bash, Read, Write tool execution with file snapshots and write serialization.
//! Follows the same patterns as rustycode's ToolExecutor but implemented directly.
//!
//! Story 4-3b: The old workspace-rooted `take_snapshot` mechanism (storing to
//! `.claude/sessions/{global_session_id}/snapshots/`) has been REPLACED by the
//! `StoragePort::snapshot_file()` protocol which stores snapshots co-located with
//! their conversation at `{sessions_dir}/{conversation_id}/snapshots/`.
//! The old path leaked across conversations and was not cleaned up by `delete_conversation()`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::adapters::skill_activation::SkillActivator;
use crate::domain::errors::ToolError;
use crate::domain::events::{AppEvent, ToolProgressEvent};
use crate::domain::models::checkpoint::CheckpointId;
use crate::domain::models::{ToolDefinition, ToolProgressConfig, ToolResult};
use crate::domain::ports::{StoragePort, ToolSetPort};
use crate::domain::services::plan_manager::PlanManager;

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
    /// Current plan file path for `exit_plan_mode` resolution.
    current_plan_file: Mutex<Option<std::path::PathBuf>>,
    /// Optional skill activator for `activate_skill` tool execution.
    #[allow(dead_code)]
    activator: Option<Arc<SkillActivator>>,
    /// Optional plan manager for `exit_plan_mode` tool execution.
    #[allow(dead_code)]
    plan_manager: Option<Arc<PlanManager>>,
    /// Optional event bus sender for emitting plan approval events.
    #[allow(dead_code)]
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<AppEvent>>,
    /// Progress event sender for long-running tool stdout tail. Story 16.9.
    progress_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<ToolProgressEvent>>>,
    /// Tool progress configuration (tail_lines cap, threshold). Story 16.9.
    tool_progress_config: Mutex<ToolProgressConfig>,
    /// Optional skill cache for `skill_view` tool execution (Story 9.6).
    #[allow(dead_code)]
    skill_cache: Option<std::sync::Arc<crate::infrastructure::skill_cache::SkillCache>>,
}

/// Context for the `stream_lines` helper, bundling the many parameters
/// that were previously passed as individual closure captures.
#[derive(Clone)]
struct StreamLinesContext {
    ring: Arc<tokio::sync::Mutex<VecDeque<String>>>,
    counter: Arc<AtomicU64>,
    last_emit: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
    accumulator: Arc<tokio::sync::Mutex<String>>,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<ToolProgressEvent>>,
    tid: String,
    tail_cap: usize,
    threshold_ms: u64,
    spawn_instant: std::time::Instant,
    emit_enabled: bool,
}

async fn stream_lines(
    stream_opt: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>,
    ctx: StreamLinesContext,
) {
    if let Some(stream) = stream_opt {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    // Decode lossily so invalid UTF-8 does not truncate the stream.
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.strip_suffix('\n').unwrap_or(&line);
                    let line = line.strip_suffix('\r').unwrap_or(line);
                    let line_owned = line.to_string();

                    // Accumulate for the final result in a single lock acquisition.
                    {
                        let mut acc = ctx.accumulator.lock().await;
                        acc.push_str(&line_owned);
                        acc.push('\n');
                    }
                    let k = ctx.counter.fetch_add(1, Ordering::Relaxed) + 1;
                    // Ring buffer
                    {
                        let mut ring_guard = ctx.ring.lock().await;
                        ring_guard.push_back(line_owned);
                        while ring_guard.len() > ctx.tail_cap {
                            ring_guard.pop_front();
                        }
                    }
                    if ctx.emit_enabled {
                        let elapsed = ctx
                            .spawn_instant
                            .elapsed()
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX);
                        if elapsed >= ctx.threshold_ms {
                            // Counter events are NOT throttled — rail render is constant-cost.
                            if let Some(ref tx) = ctx.progress_tx {
                                let _ = tx.send(ToolProgressEvent::Counter {
                                    tool_use_id: ctx.tid.clone(),
                                    k,
                                    n: k,
                                });
                            }
                            // Throttle tail emit to ~250ms
                            let now = tokio::time::Instant::now();
                            let should_emit_tail = {
                                let mut le = ctx.last_emit.lock().await;
                                match *le {
                                    None => {
                                        *le = Some(now);
                                        true
                                    }
                                    Some(prev) => {
                                        if now.duration_since(prev).as_millis() >= 250 {
                                            *le = Some(now);
                                            true
                                        } else {
                                            false
                                        }
                                    }
                                }
                            };
                            if should_emit_tail {
                                if let Some(ref tx) = ctx.progress_tx {
                                    let ring_guard = ctx.ring.lock().await;
                                    let tail_text: String =
                                        ring_guard.iter().cloned().collect::<Vec<_>>().join("\n");
                                    let _ = tx.send(ToolProgressEvent::Tail {
                                        tool_use_id: ctx.tid.clone(),
                                        text: tail_text,
                                    });
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("stream read error: {}", e);
                    break;
                }
            }
        }
    }
}

impl ToolSetAdapter {
    pub fn new(workspace_path: PathBuf, storage: Arc<dyn StoragePort>) -> Self {
        Self {
            workspace_path,
            write_locks: Arc::new(Mutex::new(HashMap::new())),
            storage,
            current_context: Mutex::new(None),
            current_plan_file: Mutex::new(None),
            activator: None,
            plan_manager: None,
            event_tx: None,
            progress_tx: Mutex::new(None),
            tool_progress_config: Mutex::new(ToolProgressConfig::default()),
            skill_cache: None,
        }
    }

    #[allow(dead_code)]
    pub fn set_activator(&mut self, activator: Arc<SkillActivator>) {
        self.activator = Some(activator);
    }

    #[allow(dead_code)]
    pub fn set_skill_cache(
        &mut self,
        cache: std::sync::Arc<crate::infrastructure::skill_cache::SkillCache>,
    ) {
        self.skill_cache = Some(cache);
    }

    #[allow(dead_code)]
    pub fn set_plan_manager(&mut self, plan_manager: Arc<PlanManager>) {
        self.plan_manager = Some(plan_manager);
    }

    #[allow(dead_code)]
    pub fn set_event_tx(&mut self, event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
        self.event_tx = Some(event_tx);
    }

    #[allow(dead_code)]
    pub async fn set_progress_tx(
        &self,
        tx: Option<tokio::sync::mpsc::UnboundedSender<ToolProgressEvent>>,
    ) {
        *self.progress_tx.lock().await = tx;
    }

    #[allow(dead_code)]
    pub async fn set_tool_progress_config(&self, cfg: ToolProgressConfig) {
        *self.tool_progress_config.lock().await = cfg;
    }

    #[allow(dead_code)]
    pub async fn set_plan_file(&self, path: Option<std::path::PathBuf>) {
        *self.current_plan_file.lock().await = path;
    }

    async fn execute_bash(
        &self,
        input: &serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let progress_tx = self.progress_tx.lock().await.clone();
        self.execute_bash_with_progress(input, cancel, "", progress_tx)
            .await
    }

    async fn execute_bash_with_progress(
        &self,
        input: &serde_json::Value,
        cancel: CancellationToken,
        tool_use_id: &str,
        progress_tx: Option<tokio::sync::mpsc::UnboundedSender<ToolProgressEvent>>,
    ) -> Result<ToolResult, ToolError> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'command' parameter".into()))?;

        let timeout_ms = input
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000)
            .min(600_000);

        let cfg = self.tool_progress_config.lock().await.clone();
        let tail_cap = cfg.tail_lines_clamped();
        let threshold_ms = cfg.threshold_ms;
        let emit_enabled = progress_tx.is_some();
        // Drop the lock before spawning child
        let _ = cfg;

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&self.workspace_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::ExecutionFailed(format!("spawn: {e}")))?;

        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();
        let spawn_instant = std::time::Instant::now();

        let ring: Arc<tokio::sync::Mutex<VecDeque<String>>> =
            Arc::new(tokio::sync::Mutex::new(VecDeque::with_capacity(tail_cap)));
        let line_counter: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
        let last_emit: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let stdout_lines: Arc<tokio::sync::Mutex<String>> =
            Arc::new(tokio::sync::Mutex::new(String::new()));
        let stderr_lines: Arc<tokio::sync::Mutex<String>> =
            Arc::new(tokio::sync::Mutex::new(String::new()));

        let tid = tool_use_id.to_string();

        let ctx = StreamLinesContext {
            ring: ring.clone(),
            counter: line_counter.clone(),
            last_emit: last_emit.clone(),

            accumulator: stdout_lines.clone(),
            progress_tx: progress_tx.clone(),
            tid: tid.clone(),
            tail_cap,
            threshold_ms,
            spawn_instant,
            emit_enabled,
        };
        let stdout_task = tokio::spawn(stream_lines(
            child_stdout.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            ctx.clone(),
        ));
        let stderr_task = tokio::spawn(stream_lines(
            child_stderr.map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            StreamLinesContext {
                accumulator: stderr_lines.clone(),
                ..ctx
            },
        ));

        let wait = child.wait();
        let timed = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), wait);

        let mut wait_error = None;
        let (exit_status, was_cancelled) = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                child.kill().await.ok();
                (None, true)
            }
            res = timed => match res {
                Ok(Ok(status)) => (Some(status), false),
                Ok(Err(e)) => {
                    wait_error = Some(ToolError::ExecutionFailed(format!("wait: {e}")));
                    (None, false)
                }
                Err(_) => {
                    child.kill().await.ok();
                    (None, false)
                }
            }
        };

        // Join reader tasks AFTER wait() returns to drain remaining buffered lines (AC8 step 6).
        // Do NOT abort first — that drops in-flight buffered data.
        let (stdout_res, stderr_res) = tokio::join!(stdout_task, stderr_task);
        if let Err(ref e) = stdout_res {
            tracing::warn!("stdout reader task failed: {}", e);
        }
        if let Err(ref e) = stderr_res {
            tracing::warn!("stderr reader task failed: {}", e);
        }

        if was_cancelled {
            return Err(ToolError::Cancelled);
        }
        if let Some(e) = wait_error {
            return Err(e);
        }
        if exit_status.is_none() {
            return Err(ToolError::Timeout);
        }

        let stdout_acc = stdout_lines.lock().await;
        let stderr_acc = stderr_lines.lock().await;
        let mut result = String::new();
        if !stdout_acc.is_empty() {
            result.push_str(&stdout_acc);
        }
        if !stderr_acc.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&stderr_acc);
        }
        if result.is_empty() {
            if let Some(s) = exit_status {
                result = format!("Command exited with status {}", s);
            }
        }
        let is_error = exit_status.is_none_or(|s| !s.success());
        Ok(ToolResult {
            tool_use_id: tid,
            content: result,
            is_error,
        })
    }

    async fn execute_read(
        &self,
        input: &serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
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

        let read_fut = tokio::fs::read(&path);

        let bytes = tokio::select! {
            res = read_fut => res.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to read '{}': {}", file_path, e))
            })?,
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
        };
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
        cancel: CancellationToken,
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
            let mkdir_fut = tokio::fs::create_dir_all(parent);
            tokio::select! {
                res = mkdir_fut => res.map_err(|e| {
                    ToolError::ExecutionFailed(format!("Failed to create directories: {}", e))
                })?,
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
            }
        }

        // Write file
        let write_fut = tokio::fs::write(&path, new_content.as_bytes());
        tokio::select! {
            res = write_fut => res.map_err(|e| {
                ToolError::ExecutionFailed(format!("Failed to write '{}': {}", file_path, e))
            })?,
            _ = cancel.cancelled() => return Err(ToolError::Cancelled),
        }

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
                parallel_safe: false,
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
                parallel_safe: true,
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
                parallel_safe: false,
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
                parallel_safe: true,
            },
            ToolDefinition {
                name: "skill_view".to_string(),
                description: "Fetch the full SKILL.md body for a named skill. \
                              Use this when L1 skill metadata is insufficient to act — the body \
                              contains the recipe, examples, and bundled resources \
                              references. Equivalent to the `read_skill` affordance in \
                              peer harnesses (gemini-cli, opencode, hermes-agent).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name as it appears in the L1 metadata listing."
                        }
                    },
                    "required": ["name"]
                }),
                parallel_safe: true,
            },
            ToolDefinition {
                name: "exit_plan_mode".to_string(),
                description: "Signal that planning is complete. Presents the plan file to the user for approval. Use only when the plan is fully written and ready for review.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": {
                            "type": "string",
                            "description": "One-sentence summary of the plan to display alongside the approval card."
                        }
                    },
                    "required": ["summary"]
                }),
                parallel_safe: false,
            },
            ToolDefinition {
                name: "propose_plan".to_string(),
                description: "Propose a structured multi-step plan to the user for approval before any execution. Use this when the request requires multiple distinct steps. Each step should have a clear title and brief description. Provide an effort estimate when you have a reasonable basis.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Concise plan title (≤80 chars)." },
                        "tasks": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 20,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "title": { "type": "string", "description": "Task title (≤120 chars)." },
                                    "description": { "type": "string", "description": "Optional one-paragraph description." },
                                    "depends_on": {
                                        "type": "array",
                                        "items": { "type": "integer", "minimum": 1 },
                                        "description": "1-indexed task numbers that must complete before this task starts. Must reference earlier tasks."
                                    }
                                },
                                "required": ["title"]
                            }
                        },
                        "estimated_tool_calls": { "type": "integer", "minimum": 0 },
                        "estimated_seconds":    { "type": "integer", "minimum": 0 }
                    },
                    "required": ["title", "tasks"]
                }),
                parallel_safe: false,
            },
        ]
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match tool_name {
            "Bash" | "bash" => self.execute_bash(&input, cancel).await,
            "Read" | "read" => self.execute_read(&input, cancel).await,
            "Write" | "write" => self.execute_write(&input, "", cancel).await,
            "activate_skill" => self.execute_activate_skill(&input).await,
            "skill_view" => self.execute_skill_view(&input, "", cancel).await,
            "exit_plan_mode" => self.execute_exit_plan_mode(&input).await,
            "propose_plan" => self.execute_propose_plan(&input).await,
            _ => Err(ToolError::NotFound(tool_name.to_string())),
        }
    }

    async fn execute_with_id(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
        progress_tx: Option<tokio::sync::mpsc::UnboundedSender<ToolProgressEvent>>,
    ) -> Result<ToolResult, ToolError> {
        match tool_name {
            "Bash" | "bash" => {
                self.execute_bash_with_progress(&input, cancel, tool_use_id, progress_tx)
                    .await
            }
            "skill_view" => self.execute_skill_view(&input, tool_use_id, cancel).await,
            _ => {
                let _ = (tool_use_id, progress_tx);
                self.execute(tool_name, input, cancel).await
            }
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
    async fn execute_exit_plan_mode(
        &self,
        input: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("Plan ready for review.");

        let plan_path = self.current_plan_file.lock().await.clone();
        let plan_path = plan_path.unwrap_or_default();

        let contents = if let Some(ref pm) = self.plan_manager {
            pm.read_plan(&plan_path).await.unwrap_or_default()
        } else {
            String::new()
        };

        if let Some(ref tx) = self.event_tx {
            let context = self.current_context.lock().await;
            let conversation_id = context
                .as_ref()
                .map(|c| c.conversation_id.clone())
                .unwrap_or_default();
            let _ = tx.send(AppEvent::PlanApprovalRequested {
                conversation_id,
                plan_path,
                contents,
                summary: summary.to_string(),
            });
        }

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "Plan sent for user approval.".to_string(),
            is_error: false,
        })
    }

    async fn execute_propose_plan(
        &self,
        input: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        use crate::domain::services::plan_parser::parse_plan_input;

        let plan_id = nanoid::nanoid!();
        let plan = parse_plan_input(input, &plan_id)?;

        if let Some(ref tx) = self.event_tx {
            let context = self.current_context.lock().await;
            let conversation_id = context
                .as_ref()
                .map(|c| c.conversation_id.clone())
                .unwrap_or_default();
            let _ = tx.send(AppEvent::PlanProposed {
                conversation_id,
                plan,
            });
        } else {
            tracing::warn!(
                "execute_propose_plan: event_tx is None — plan parsed but never proposed"
            );
        }

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "Plan proposed for user approval.".to_string(),
            is_error: false,
        })
    }

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

    async fn execute_skill_view(
        &self,
        input: &serde_json::Value,
        tool_use_id: &str,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::ExecutionFailed(
                "skill_view requires 'name' string argument".into(),
            ))?;

        let cache = self.skill_cache.as_ref().ok_or_else(|| {
            ToolError::ExecutionFailed(
                "skill cache not initialized — composition error".into(),
            )
        })?;

        let body = cache.body(name).await.map_err(|e| {
            ToolError::ExecutionFailed(format!(
                "skill_view: failed to fetch body for '{}': {}",
                name, e
            ))
        })?;

        let source: Option<crate::domain::models::SkillSource> = cache.source(name).await.ok();
        let trust_attr = match source {
            Some(s) if s.priority() < 3 => " trust=\"workspace\"",
            _ => "",
        };

        Ok(ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: format!(
                "<skill name=\"{}\"{}>\n{}\n</skill>",
                name, trust_attr, body
            ),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::filesystem::FileSystemStorage;

    fn test_cancel() -> CancellationToken {
        CancellationToken::new()
    }

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
            .execute(
                "Bash",
                serde_json::json!({"command": "echo hello"}),
                test_cancel(),
            )
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
                test_cancel(),
            )
            .await
            .unwrap();
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line2"));
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_read_tool_preserves_binary_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("binary.bin");
        std::fs::write(&file, [0xFFu8, 0xFE, 0x00, 0x41, b'\n', 0xC3, 0x28]).unwrap();

        let adapter = make_adapter(tmp.path());
        let result = adapter
            .execute(
                "Read",
                serde_json::json!({"file_path": file.to_str().unwrap()}),
                test_cancel(),
            )
            .await
            .expect("Read must not fail on binary content");

        assert!(
            !result.is_error,
            "binary read should succeed via lossy decode"
        );
        assert!(
            result.content.contains('A'),
            "ASCII content must survive lossy decode"
        );
        assert!(
            result.content.contains('\u{FFFD}'),
            "invalid UTF-8 should map to replacement chars"
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
                test_cancel(),
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
        use crate::domain::models::conversation::generate_conversation_id;

        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join(".claude").join("sessions");
        let storage: Arc<dyn StoragePort> = Arc::new(FileSystemStorage::new(sessions_dir.clone()));
        let adapter = ToolSetAdapter::new(tmp.path().to_path_buf(), Arc::clone(&storage));

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
                synthetic: false,
                images: vec![],
            }],
            turns: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
        };
        storage.save_conversation(&conv).await.unwrap();

        let cp = storage.create_checkpoint(&conv_id).await.unwrap();
        adapter.set_execution_context(conv_id.clone(), cp, 0).await;

        let file = tmp.path().join("existing.txt");
        std::fs::write(&file, "original content").unwrap();

        adapter
            .execute(
                "Write",
                serde_json::json!({
                    "file_path": file.to_str().unwrap(),
                    "content": "new content"
                }),
                test_cancel(),
            )
            .await
            .unwrap();

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

        assert_eq!(snapshot["schema_version"].as_u64().unwrap(), 3);
        assert_eq!(snapshot["conversation_id"].as_str().unwrap(), conv_id);
        assert!(snapshot["file_existed"].as_bool().unwrap());
        assert!(
            snapshot["original_hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );

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
        let result = adapter
            .execute("UnknownTool", serde_json::json!({}), test_cancel())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_invalid_input_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let adapter = make_adapter(tmp.path());
        let result = adapter
            .execute("Bash", serde_json::json!({}), test_cancel())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bash_cancel_returns_cancelled() {
        let dir = std::env::current_dir().unwrap();
        let adapter = make_adapter(&dir);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = adapter
            .execute("Bash", serde_json::json!({"command": "sleep 10"}), cancel)
            .await;
        assert!(matches!(result, Err(ToolError::Cancelled)));
    }

    #[tokio::test]
    async fn test_read_cancel_returns_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "content").unwrap();
        let adapter = make_adapter(tmp.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = adapter
            .execute(
                "Read",
                serde_json::json!({"file_path": file.to_str().unwrap()}),
                cancel,
            )
            .await;
        assert!(matches!(result, Err(ToolError::Cancelled)));
    }

    #[tokio::test]
    async fn test_write_cancel_returns_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        let adapter = make_adapter(tmp.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = adapter
            .execute(
                "Write",
                serde_json::json!({"file_path": file.to_str().unwrap(), "content": "x"}),
                cancel,
            )
            .await;
        assert!(matches!(result, Err(ToolError::Cancelled)));
        assert!(!file.exists(), "file must not be created when cancelled");
    }

    // Story 16.9: threshold gate tests (AC11)

    #[tokio::test]
    async fn execute_bash_no_events_below_threshold() {
        let dir = std::env::current_dir().unwrap();
        let adapter = make_adapter(&dir);
        adapter
            .set_tool_progress_config(ToolProgressConfig {
                live_tail: true,
                tail_lines: 4,
                threshold_ms: 3000,
            })
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Script totals ~1s — well below the 3s threshold
        let _result = adapter
            .execute_bash_with_progress(
                &serde_json::json!({"command": "echo a; sleep 0.5; echo b; sleep 0.5; echo c"}),
                CancellationToken::new(),
                "test-below",
                Some(tx),
            )
            .await
            .unwrap();

        assert!(
            rx.try_recv().is_err(),
            "zero events should be emitted below threshold"
        );
    }

    #[tokio::test]
    async fn execute_bash_streams_lines_after_threshold() {
        let dir = std::env::current_dir().unwrap();
        let adapter = make_adapter(&dir);
        adapter
            .set_tool_progress_config(ToolProgressConfig {
                live_tail: true,
                tail_lines: 4,
                threshold_ms: 3000,
            })
            .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        // Script totals ~4s — exceeds the 3s threshold
        let _result = adapter
            .execute_bash_with_progress(
                &serde_json::json!({"command": "for i in 1 2 3 4 5 6 7 8 9 10; do echo line $i; sleep 0.4; done"}),
                CancellationToken::new(),
                "test-above",
                Some(tx),
            )
            .await
            .unwrap();

        let mut got_counter = false;
        let mut got_tail = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                ToolProgressEvent::Counter { .. } => got_counter = true,
                ToolProgressEvent::Tail { .. } => got_tail = true,
            }
        }
        assert!(got_counter, "should emit >=1 Counter after threshold");
        assert!(got_tail, "should emit >=1 Tail after threshold");
    }

    #[tokio::test]
    async fn bash_failure_mid_stream_preserves_full_stdout() {
        let dir = std::env::current_dir().unwrap();
        let adapter = make_adapter(&dir);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        adapter.set_progress_tx(Some(tx.clone())).await;
        adapter
            .set_tool_progress_config(ToolProgressConfig {
                live_tail: true,
                tail_lines: 4,
                threshold_ms: 500, // low threshold so events fire
            })
            .await;

        let result = adapter
            .execute_bash_with_progress(
                &serde_json::json!({"command": "echo a; sleep 0.5; echo b; sleep 0.5; echo c; sleep 1; echo d; echo e; echo f; exit 1"}),
                CancellationToken::new(),
                "test-failure",
                Some(tx),
            )
            .await
            .unwrap();

        assert!(result.is_error, "exit 1 must mark ToolResult as error");
        for line in &["a", "b", "c", "d", "e", "f"] {
            assert!(
                result.content.contains(line),
                "full stdout must contain '{}' — not just ring-buffered last 4",
                line
            );
        }
    }
}
