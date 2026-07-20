//! MCP Tasks extension (SEP-2663, 2026-07-28 RC) wire types.
//!
//! Hand-rolled from captured reference-implementation bytes — see
//! `_bmad-output/planning-artifacts/research/mcp-tasks-17-5a-spike-2026-07-19/SPIKE_REPORT.md`
//! (mcpkit @ `02cfbe0` `examples/tasks-v2`; schema pin ext-tasks @ `2c1425d`).
//! `rmcp` 1.7.0's `rmcp::model::task` types implement the SUPERSEDED
//! SEP-1319/SEP-1686 shape and must never touch this wire (ADR-17-5-01 D1).
//! The one exception is [`rmcp::model::TaskStatus`], whose five snake_case
//! variants are byte-correct against the RC and the capture.
//!
//! Scope (17.5a): `tasks/get` and `tasks/cancel` only. `tasks/update` is
//! 17.5b's method; there is deliberately no type for it here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Reused from rmcp — the ONE type verified byte-correct against the RC
/// (five snake_case statuses: working/input_required/completed/failed/cancelled).
pub use rmcp::model::TaskStatus;

/// Wire method names (sent via `ClientRequest::CustomRequest`).
pub const TASKS_GET_METHOD: &str = "tasks/get";
pub const TASKS_CANCEL_METHOD: &str = "tasks/cancel";

/// `resultType` discriminator on a `tools/call` reply that created a task (R-13).
pub const RESULT_TYPE_TASK: &str = "task";

/// `resultType` discriminator on a `tasks/get`/`tasks/cancel` reply (the RC's
/// value for a settled response; a `tasks/cancel` ack is `{"resultType":"complete"}`).
pub const RESULT_TYPE_COMPLETE: &str = "complete";

/// Extension id advertised in per-request `_meta` (R-13; SEP-2575 key).
pub const TASKS_EXTENSION_ID: &str = "io.modelcontextprotocol/tasks";
/// `_meta` key carrying the per-request client-capabilities override.
pub const PER_REQUEST_CLIENT_CAPS_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";

/// Wrapper key inserted by the byte-level transport shim
/// (`task_transport.rs`) so rmcp's untagged `ServerResult` decode cannot
/// misclassify a task-shaped payload as its superseded `GetTaskResult`
/// (ADR-17-5-01 D1 amendment — measured, not theoretical).
pub const TASK_WRAPPER_KEY: &str = "$rustainMcpTask";

/// Flat task record shared by the creation reply and every `tasks/get` reply.
///
/// Captured shape (create + get): `taskId`, `status`, `createdAt`,
/// `lastUpdatedAt`, `ttlMs`, `pollIntervalMs`, optional `statusMessage`.
/// The RC flattens these at the top level (no nested `task` object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTask {
    /// Server-chosen task identifier — untrusted input, sanitize before use.
    pub task_id: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// RFC3339 timestamp (captured as a string; never parsed to a clock type).
    pub created_at: String,
    pub last_updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
}

/// Reply to a task-creating `tools/call` (captured: `resultType: "task"` +
/// flat task fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskReply {
    pub result_type: String,
    #[serde(flatten)]
    pub task: McpTask,
}

impl CreateTaskReply {
    /// R-13 detection rule: a `tools/call` reply is a task iff
    /// `resultType == "task"`. Anything else is a normal tool result.
    pub fn is_task(&self) -> bool {
        self.result_type == RESULT_TYPE_TASK
    }
}

/// Reply to `tasks/get` (captured: `resultType: "complete"` + flat task
/// fields + inlined terminal payloads).
///
/// `requestState` appears on this server's responses (spike drift D1) and is
/// deliberately NOT modeled — it is response-side noise; correlation is
/// taskId + input-request key. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGetReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
    #[serde(flatten)]
    pub task: McpTask,
    /// Inlined tool result when `status == "completed"` (CallToolResult shape,
    /// including `isError: true` for tool-level failures — R-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Inlined structured error when `status == "failed"` (JSON-RPC-error-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskError>,
    /// Outstanding input requests when `status == "input_required"`.
    /// Keys are unique over the task lifetime. Decoded, never interpreted,
    /// in 17.5a (the resume path is 17.5b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_requests: Option<BTreeMap<String, InputRequest>>,
}

/// Structured error inlined on a `failed` task (captured: `{code, message}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// One outstanding input request envelope (captured: `{method, params}`
/// where params carries an opaque request body such as `elicitation/create`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRequest {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Ack reply for `tasks/cancel` (and, in 17.5b, `tasks/update`).
/// Captured as `{"resultType": "complete"}` — an empty payload we decode
/// tolerantly (drift D2: the README's "empty {}" claim is wrong on the wire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
}

impl TaskAck {
    /// A genuine cancel ack carries `resultType: "complete"` (drift D2 — the
    /// README's "empty {}" is wrong on the wire). Anything else (a legacy
    /// `CancelTaskResult` that the shim wrapped, or a `pending`/task-shaped
    /// payload whose unknown fields serde silently drops) is NOT an ack and
    /// must be refused so the driver never forges a `Cancelled` transition.
    pub fn is_complete(&self) -> bool {
        self.result_type.as_deref() == Some(RESULT_TYPE_COMPLETE)
    }
}

/// Params for `tasks/get` and `tasks/cancel` (identical shape: `{taskId}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskIdParams {
    pub task_id: String,
}

/// Per-request `_meta` advertising the Tasks extension (R-13). There is no
/// client opt-in field in `tools/call` params — this `_meta` is the whole
/// advertisement, and the server decides whether to create a task.
pub fn tasks_extension_meta() -> Value {
    serde_json::json!({
        PER_REQUEST_CLIENT_CAPS_META_KEY: {
            "extensions": { TASKS_EXTENSION_ID: {} }
        }
    })
}

/// Transport-shim detection: is this response `result` task-shaped?
///
/// True when the result is the flat task record — either the creation
/// discriminator (`resultType == "task"`) or the `tasks/get` shape
/// (`taskId` + `status` + both timestamps). A plain `CallToolResult`
/// (`content: [...]`, no task fields) is false and passes through untouched.
pub fn is_task_shaped_result(result: &Value) -> bool {
    let Some(obj) = result.as_object() else {
        return false;
    };
    if obj.get("resultType").and_then(Value::as_str) == Some(RESULT_TYPE_TASK) {
        return true;
    }
    obj.contains_key("taskId")
        && obj.contains_key("status")
        && obj.contains_key("createdAt")
        && obj.contains_key("lastUpdatedAt")
}

/// Wrap a task-shaped result so rmcp's untagged `ServerResult` decode falls
/// through to `CustomResult` (the wrapper matches no typed variant and fails
/// `EmptyResult`'s `deny_unknown_fields`).
pub fn wrap_task_result(result: Value) -> Value {
    serde_json::json!({ TASK_WRAPPER_KEY: result })
}

/// Inverse of [`wrap_task_result`]. Returns the raw task payload when the
/// value carries the wrapper, else `None`.
pub fn unwrap_task_result(value: &Value) -> Option<Value> {
    value.get(TASK_WRAPPER_KEY).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/mcp/{name}", env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"));
        let envelope: Value = serde_json::from_str(&raw).expect("envelope parses");
        envelope
            .get("result")
            .cloned()
            .expect("envelope has result")
    }

    #[test]
    fn captured_fixtures_match_the_manifest_sha256_provenance() {
        // Task 0b / Class-C: the wire types are pinned to the CAPTURED mcpkit
        // bytes. Verifying each fixture against its recorded sha256 kills the
        // "edit a fixture together with its codec expectation" mutant — a silent
        // fixture edit no longer stays green.
        use sha2::{Digest, Sha256};
        let dir = format!("{}/tests/fixtures/mcp", env!("CARGO_MANIFEST_DIR"));
        let manifest_raw = std::fs::read_to_string(format!("{dir}/manifest.json"))
            .expect("manifest.json is readable");
        let manifest: serde_json::Map<String, Value> =
            serde_json::from_str(&manifest_raw).expect("manifest parses");
        assert!(
            !manifest.is_empty(),
            "manifest must pin at least one captured fixture"
        );
        for (name, entry) in &manifest {
            let expected = entry
                .get("sha256")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("manifest entry {name} has no sha256"));
            let bytes = std::fs::read(format!("{dir}/{name}"))
                .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
            let actual = hex::encode(Sha256::digest(&bytes));
            assert_eq!(
                actual, expected,
                "fixture {name} drifted from its captured sha256 — re-capture (Task 0b) \
                 or restore the bytes; never hand-edit a pinned capture"
            );
        }
    }

    #[test]
    fn create_task_fixture_decodes_flat_with_result_type() {
        let reply: CreateTaskReply =
            serde_json::from_value(fixture("create_task.json")).expect("decodes");
        assert!(reply.is_task());
        assert_eq!(reply.task.status, TaskStatus::Working);
        assert!(reply.task.task_id.starts_with("task-"));
        assert_eq!(reply.task.ttl_ms, Some(300_000));
        assert_eq!(reply.task.poll_interval_ms, Some(1_000));
        assert!(!reply.task.created_at.is_empty());
        assert!(!reply.task.last_updated_at.is_empty());
    }

    #[test]
    fn get_working_fixture_decodes() {
        let reply: TaskGetReply =
            serde_json::from_value(fixture("get_working.json")).expect("decodes");
        assert_eq!(reply.task.status, TaskStatus::Working);
        assert!(reply.result.is_none());
        assert!(reply.error.is_none());
        assert!(reply.input_requests.is_none());
    }

    #[test]
    fn get_completed_fixture_decodes_with_inlined_result() {
        let reply: TaskGetReply =
            serde_json::from_value(fixture("get_completed.json")).expect("decodes");
        assert_eq!(reply.task.status, TaskStatus::Completed);
        let result = reply.result.expect("completed inlines the tool result");
        assert!(result.get("content").is_some(), "CallToolResult shape");
    }

    #[test]
    fn get_input_required_fixture_decodes_keyed_input_requests() {
        let reply: TaskGetReply =
            serde_json::from_value(fixture("get_input_required.json")).expect("decodes");
        assert_eq!(reply.task.status, TaskStatus::InputRequired);
        let requests = reply
            .input_requests
            .expect("input_required carries requests");
        let (key, req) = requests.iter().next().expect("at least one key");
        assert!(!key.is_empty(), "key is the lifetime-scoped correlation id");
        assert_eq!(req.method, "elicitation/create");
        assert!(req.params.get("requestedSchema").is_some());
    }

    #[test]
    fn cancel_ack_fixture_decodes_tolerantly() {
        let ack: TaskAck = serde_json::from_value(fixture("cancel_ack.json")).expect("decodes");
        assert_eq!(ack.result_type.as_deref(), Some("complete"));
    }

    #[test]
    fn get_cancelled_fixture_decodes_with_status_message() {
        let reply: TaskGetReply =
            serde_json::from_value(fixture("get_cancelled.json")).expect("decodes");
        assert_eq!(reply.task.status, TaskStatus::Cancelled);
        assert!(reply.task.status_message.is_some());
    }

    #[test]
    fn get_failed_fixture_decodes_inlined_error() {
        let reply: TaskGetReply =
            serde_json::from_value(fixture("get_failed.json")).expect("decodes");
        assert_eq!(reply.task.status, TaskStatus::Failed);
        let error = reply.error.expect("failed inlines a structured error");
        assert_eq!(error.code, -32603);
        assert!(!error.message.is_empty());
    }

    #[test]
    fn get_completed_iserror_fixture_decodes_tool_error_as_completed() {
        // R-14: isError:true is a COMPLETED task, never a Failed one.
        let reply: TaskGetReply =
            serde_json::from_value(fixture("get_completed_iserror.json")).expect("decodes");
        assert_eq!(reply.task.status, TaskStatus::Completed);
        let result = reply.result.expect("completed inlines the tool result");
        assert_eq!(result.get("isError").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn task_shaped_detection_accepts_all_task_fixtures() {
        for name in [
            "create_task.json",
            "get_working.json",
            "get_completed.json",
            "get_input_required.json",
            "get_cancelled.json",
            "get_failed.json",
            "get_completed_iserror.json",
        ] {
            assert!(
                is_task_shaped_result(&fixture(name)),
                "{name} must be task-shaped"
            );
        }
    }

    #[test]
    fn task_shaped_detection_rejects_plain_tool_results_and_acks() {
        assert!(!is_task_shaped_result(&fixture("call_sync_reply.json")));
        assert!(!is_task_shaped_result(&fixture("cancel_ack.json")));
        assert!(!is_task_shaped_result(&serde_json::json!(null)));
        assert!(!is_task_shaped_result(&serde_json::json!({"content": []})));
    }

    #[test]
    fn wrapper_round_trips_and_is_not_task_shaped() {
        let raw = fixture("get_completed.json");
        let wrapped = wrap_task_result(raw.clone());
        // The wrapper itself must NOT match the shim's detection (no top-level
        // task fields) — otherwise it would double-wrap.
        assert!(!is_task_shaped_result(&wrapped));
        assert_eq!(unwrap_task_result(&wrapped), Some(raw));
        assert_eq!(
            unwrap_task_result(&serde_json::json!({"content": []})),
            None
        );
    }

    #[test]
    fn task_id_params_serialize_camel_case() {
        let params = TaskIdParams {
            task_id: "task-abc".into(),
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            serde_json::json!({"taskId": "task-abc"})
        );
    }

    #[test]
    fn extension_meta_matches_captured_shape() {
        let meta = tasks_extension_meta();
        assert_eq!(
            meta,
            serde_json::json!({
                "io.modelcontextprotocol/clientCapabilities": {
                    "extensions": { "io.modelcontextprotocol/tasks": {} }
                }
            })
        );
    }
}
