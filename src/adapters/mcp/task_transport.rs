//! Byte-level stdio transport shim + the narrow task-lifecycle transport seam.
//!
//! Two pieces:
//!
//! 1. [`TaskGuardTransport`] — a `Transport<RoleClient>` over the child's
//!    stdio that rewrites task-shaped JSON-RPC response payloads before
//!    rmcp's typed decode. rmcp 1.7.0 deserializes every response into
//!    `ServerResult` as an untagged union matched by SHAPE, and its
//!    superseded `GetTaskResult` variant matches any flat task record —
//!    silently dropping `resultType`, `result`, `error`, and
//!    `inputRequests` (ADR-17-5-01 D1 amendment; measured against the
//!    captured fixtures). Wrapping task-shaped results under
//!    [`TASK_WRAPPER_KEY`] forces the decode to `CustomResult`, from which
//!    our own serde types (`tasks.rs`) decode. Every non-task message —
//!    normal `tools/call` replies, notifications, errors — passes through
//!    byte-identically.
//!
//! 2. [`McpTaskTransport`] — the narrow seam the poll loop drives, mirroring
//!    `A2aTaskTransport` (`a2a/lifecycle.rs`) so scripted doubles and the
//!    real peer share one trait. Both methods travel as
//!    `ClientRequest::CustomRequest`; `tasks/update` rides the same seam
//!    (`tasks_update`, added in 17.5b — the helper is params-generic).

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{ClientRequest, CustomRequest, ServerResult};
use rmcp::service::{Peer, RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::error::McpError;
use super::tasks::{
    self, InputResponse, TASKS_CANCEL_METHOD, TASKS_GET_METHOD, TASKS_UPDATE_METHOD, TaskAck,
    TaskGetReply, TaskIdParams, TasksUpdateParams,
};

/// Byte-level stdio transport that guards the Tasks wire shapes.
///
/// Owns the child process handle so dropping the transport drops the child
/// (the spawning `Command` carries `kill_on_drop(true)`, matching the prior
/// `TokioChildProcess` behavior; stderr is inherited, as rmcp's builder
/// defaults do).
pub struct TaskGuardTransport {
    read: tokio::io::BufReader<tokio::process::ChildStdout>,
    write: Arc<Mutex<tokio::process::ChildStdin>>,
    child: Option<tokio::process::Child>,
    read_buf: Vec<u8>,
}

impl TaskGuardTransport {
    /// Spawn `cmd` with piped stdin/stdout and inherited stderr — the exact
    /// stdio shape rmcp's `TokioChildProcessBuilder` uses by default.
    pub fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<Self> {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout was already taken"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("child stdin was already taken"))?;
        Ok(Self {
            read: tokio::io::BufReader::new(stdout),
            write: Arc::new(Mutex::new(stdin)),
            child: Some(child),
            read_buf: Vec::new(),
        })
    }
}

/// Wrap a task-shaped response `result` under [`tasks::TASK_WRAPPER_KEY`] in
/// place. Anything that is not a JSON-RPC response with a task-shaped result
/// is left untouched.
fn guard_response(value: &mut Value) {
    let is_task = value
        .as_object()
        .and_then(|obj| obj.get("result"))
        .is_some_and(tasks::is_task_shaped_result);
    if !is_task {
        return;
    }
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if let Some(raw) = obj.insert("result".into(), Value::Null) {
        obj.insert("result".into(), tasks::wrap_task_result(raw));
    }
}

impl Transport<RoleClient> for TaskGuardTransport {
    type Error = std::io::Error;

    #[allow(refining_impl_trait_reachable)]
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = Arc::clone(&self.write);
        let bytes = serde_json::to_vec(&item).map(|mut b| {
            b.push(b'\n');
            b
        });
        async move {
            let bytes =
                bytes.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let mut guard = write.lock().await;
            guard.write_all(&bytes).await?;
            guard.flush().await
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            self.read_buf.clear();
            match self.read.read_until(b'\n', &mut self.read_buf).await {
                Ok(0) => return None, // EOF: child closed stdout
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "MCP child stdout read failed; closing transport");
                    return None;
                }
            }
            let line = self.read_buf.as_slice();
            let mut value: Value = match serde_json::from_slice(line) {
                Ok(value) => value,
                Err(error) => {
                    // Mirror rmcp's recover-from-parse-error: a malformed line
                    // must not kill the session.
                    tracing::warn!(%error, "skipping malformed JSON-RPC line from MCP child");
                    continue;
                }
            };
            guard_response(&mut value);
            match serde_json::from_value::<RxJsonRpcMessage<RoleClient>>(value) {
                Ok(message) => return Some(message),
                Err(error) => {
                    tracing::warn!(%error, "skipping undecodable JSON-RPC message from MCP child");
                    continue;
                }
            }
        }
    }

    #[allow(refining_impl_trait_reachable)]
    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = Arc::clone(&self.write);
        let child = self.child.take();
        async move {
            // Close stdin first so a cooperative child sees EOF and exits.
            {
                let mut guard = write.lock().await;
                let _ = guard.shutdown().await;
            }
            // AC5 (no leaked child): `kill_on_drop` only *initiates* a kill and
            // Tokio documents reaping as best-effort, so close explicitly waits
            // for exit — bounded — and hard-kills + reaps a straggler.
            if let Some(mut child) = child {
                match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
                    Ok(_) => {}
                    Err(_) => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                }
            }
            Ok(())
        }
    }
}

/// The narrow task-lifecycle seam the MCP task driver polls through.
/// Production impl is [`PeerTaskTransport`]; tests use scripted doubles
/// (the `A2aTaskTransport` pattern).
#[async_trait::async_trait]
pub trait McpTaskTransport: Send + Sync {
    /// `tasks/get` — returns OUR decoded reply type, never rmcp's superseded
    /// `GetTaskResult`.
    async fn tasks_get(&self, task_id: &str) -> Result<TaskGetReply, McpError>;
    /// `tasks/cancel` — cooperative; the ack is all the driver needs (R-15).
    async fn tasks_cancel(&self, task_id: &str) -> Result<TaskAck, McpError>;
    /// `tasks/update` — resume a parked task by answering its outstanding
    /// input requests (FR147 / AC1). Correlation is `taskId` + key (R-6); the
    /// driver refuses unknown keys before this call. Ack is tolerant (R-1):
    /// both `{}` and `{"resultType":"complete"}` are accepted — the next
    /// `tasks/get` observes whether the task actually left `input_required`.
    async fn tasks_update(
        &self,
        task_id: &str,
        responses: std::collections::BTreeMap<String, InputResponse>,
    ) -> Result<TaskAck, McpError>;
}

/// Production [`McpTaskTransport`] over the live rmcp peer.
pub struct PeerTaskTransport {
    peer: Peer<RoleClient>,
}

impl PeerTaskTransport {
    pub fn new(peer: Peer<RoleClient>) -> Self {
        Self { peer }
    }

    /// Send a `{taskId}`-parametrized Tasks method via `CustomRequest`,
    /// advertising the extension in per-request `_meta` (R-13). Unwraps the
    /// transport shim's wrapper; refuses rmcp's typed task variants loudly —
    /// a legacy (SEP-1686) reply must never decode into the superseded shape.
    async fn send_task_request(&self, method: &str, task_id: &str) -> Result<Value, McpError> {
        let params = serde_json::to_value(TaskIdParams {
            task_id: task_id.to_string(),
        })
        .map_err(|e| McpError::TaskProtocol(format!("params serialize: {e}")))?;
        self.send_task_request_with_params(method, params).await
    }

    /// Send an arbitrary Tasks method whose `params` are caller-built, via
    /// `CustomRequest`. Stamps the per-request extension `_meta`, sends the
    /// request, unwraps the transport shim's wrapper, and maps transport
    /// failures to [`McpError::TransportClosed`]. Used by `tasks/update`
    /// (17.5b) whose params are `{taskId, inputResponses}`, not `{taskId}`.
    async fn send_task_request_with_params(
        &self,
        method: &str,
        mut params: Value,
    ) -> Result<Value, McpError> {
        if let Some(obj) = params.as_object_mut() {
            obj.insert("_meta".into(), tasks::tasks_extension_meta());
        }
        let request = ClientRequest::CustomRequest(CustomRequest::new(method, Some(params)));
        match self.peer.send_request(request).await {
            Ok(ServerResult::CustomResult(value)) => {
                Ok(tasks::unwrap_task_result(&value.0).unwrap_or(value.0))
            }
            Ok(_other) => Err(McpError::TaskProtocol(format!(
                "{method}: server replied in a legacy typed shape; refusing to decode \
                 the superseded SEP-1686 form"
            ))),
            Err(e) => {
                let err_str = format!("{e}");
                if err_str.contains("transport") || err_str.contains("closed") {
                    Err(McpError::TransportClosed(err_str))
                } else {
                    Err(McpError::TaskProtocol(format!("{method}: {err_str}")))
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl McpTaskTransport for PeerTaskTransport {
    async fn tasks_get(&self, task_id: &str) -> Result<TaskGetReply, McpError> {
        let value = self.send_task_request(TASKS_GET_METHOD, task_id).await?;
        serde_json::from_value(value)
            .map_err(|e| McpError::TaskProtocol(format!("tasks/get decode: {e}")))
    }

    async fn tasks_cancel(&self, task_id: &str) -> Result<TaskAck, McpError> {
        let value = self.send_task_request(TASKS_CANCEL_METHOD, task_id).await?;
        let ack: TaskAck = serde_json::from_value(value)
            .map_err(|e| McpError::TaskProtocol(format!("tasks/cancel decode: {e}")))?;
        // R-15 / P9: only a real `resultType:"complete"` ack may gate the local
        // `Cancelled` transition. A legacy `CancelTaskResult` (wrapped by the
        // shim) or a `pending` payload decodes into `TaskAck` with unknown
        // fields dropped — refuse it rather than forge a cancellation.
        if !ack.is_complete() {
            return Err(McpError::TaskProtocol(format!(
                "tasks/cancel for {task_id}: reply is not a `resultType:\"complete\"` ack"
            )));
        }
        Ok(ack)
    }

    async fn tasks_update(
        &self,
        task_id: &str,
        responses: std::collections::BTreeMap<String, InputResponse>,
    ) -> Result<TaskAck, McpError> {
        let params = serde_json::to_value(TasksUpdateParams {
            task_id: task_id.to_string(),
            input_responses: responses,
        })
        .map_err(|e| McpError::TaskProtocol(format!("params serialize: {e}")))?;
        let value = self
            .send_task_request_with_params(TASKS_UPDATE_METHOD, params)
            .await?;
        decode_tasks_update_ack(task_id, value)
    }
}

fn decode_tasks_update_ack(task_id: &str, value: serde_json::Value) -> Result<TaskAck, McpError> {
    // R-1: accept exactly the two measured ack shapes. Serde's default
    // unknown-field tolerance must not turn `pending` or arbitrary objects
    // into an acknowledgement that resumes the node.
    let Some(object) = value.as_object() else {
        return Err(McpError::TaskProtocol(
            "tasks/update ack must be an object".into(),
        ));
    };
    if object.is_empty() {
        return Ok(TaskAck { result_type: None });
    }
    if object.len() == 1
        && object.get("resultType").and_then(serde_json::Value::as_str) == Some("complete")
    {
        return Ok(TaskAck {
            result_type: Some("complete".into()),
        });
    }
    Err(McpError::TaskProtocol(format!(
        "tasks/update for {task_id}: reply is neither `{{}}` nor a `resultType:\"complete\"` ack"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_update_ack_accepts_only_empty_or_complete() {
        assert!(decode_tasks_update_ack("task-1", serde_json::json!({})).is_ok());
        assert!(
            decode_tasks_update_ack("task-1", serde_json::json!({"resultType": "complete"}))
                .is_ok()
        );
        for malformed in [
            serde_json::json!({"resultType": "pending"}),
            serde_json::json!({"unexpected": true}),
            serde_json::json!({"resultType": "complete", "unexpected": true}),
            serde_json::json!([]),
        ] {
            assert!(
                decode_tasks_update_ack("task-1", malformed).is_err(),
                "malformed update ack must be refused"
            );
        }
    }
    #[test]
    fn guard_wraps_task_shaped_results_only() {
        // tasks/get reply shape: wrapped.
        let mut get_reply = serde_json::json!({
            "jsonrpc": "2.0", "id": 5,
            "result": {"resultType": "complete", "taskId": "task-1", "status": "working",
                       "createdAt": "t", "lastUpdatedAt": "t"}
        });
        guard_response(&mut get_reply);
        let result = &get_reply["result"];
        assert!(result.get(tasks::TASK_WRAPPER_KEY).is_some());

        // Creation reply shape: wrapped (resultType discriminator).
        let mut create_reply = serde_json::json!({
            "jsonrpc": "2.0", "id": 4,
            "result": {"resultType": "task", "taskId": "task-1", "status": "working",
                       "createdAt": "t", "lastUpdatedAt": "t", "ttlMs": 300000}
        });
        guard_response(&mut create_reply);
        assert!(
            create_reply["result"]
                .get(tasks::TASK_WRAPPER_KEY)
                .is_some()
        );
    }

    #[test]
    fn guard_passes_non_task_messages_byte_identically() {
        // A plain tools/call reply: untouched.
        let mut call_reply = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"content": [{"type": "text", "text": "hi"}], "isError": false}
        });
        let before = call_reply.clone();
        guard_response(&mut call_reply);
        assert_eq!(call_reply, before);

        // A cancel ack (resultType: complete, no task fields): untouched.
        let mut ack = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "result": {"resultType": "complete"}
        });
        let before = ack.clone();
        guard_response(&mut ack);
        assert_eq!(ack, before);

        // A JSON-RPC error: untouched.
        let mut error = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "error": {"code": -32601, "message": "method not found"}
        });
        let before = error.clone();
        guard_response(&mut error);
        assert_eq!(error, before);

        // A server notification: untouched.
        let mut notif = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/progress", "params": {}
        });
        let before = notif.clone();
        guard_response(&mut notif);
        assert_eq!(notif, before);
    }

    /// The load-bearing guarantee (ADR-17-5-01 D1 amendment): after the
    /// guard, rmcp's untagged `ServerResult` decode lands on `CustomResult`
    /// for a task-shaped payload — never on the superseded `GetTaskResult`.
    #[test]
    fn guarded_task_reply_decodes_as_custom_result_not_get_task_result() {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/mcp/get_completed.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut envelope: Value = serde_json::from_str(&raw).unwrap();
        guard_response(&mut envelope);
        let result = envelope.get("result").cloned().unwrap();
        let decoded: ServerResult = serde_json::from_value(result).expect("decodes");
        match decoded {
            ServerResult::CustomResult(value) => {
                let raw = tasks::unwrap_task_result(&value.0).expect("wrapped");
                let reply: TaskGetReply = serde_json::from_value(raw).expect("our type decodes");
                assert_eq!(reply.task.status, tasks::TaskStatus::Completed);
                assert!(reply.result.is_some(), "inlined tool result survives");
            }
            other => panic!("guard failed: decoded as {other:?}, not CustomResult"),
        }
    }

    /// Unguarded, the same fixture decodes as rmcp's superseded GetTaskResult
    /// and loses the inlined result — the exact bug the shim exists to kill.
    #[test]
    fn unguarded_task_reply_loses_inlined_payload() {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/mcp/get_completed.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let envelope: Value = serde_json::from_str(&raw).unwrap();
        let result = envelope.get("result").cloned().unwrap();
        let decoded: ServerResult = serde_json::from_value(result).expect("decodes");
        assert!(
            matches!(decoded, ServerResult::GetTaskResult(_)),
            "rmcp decodes task-shaped replies into its superseded shape (measured)"
        );
    }
}
