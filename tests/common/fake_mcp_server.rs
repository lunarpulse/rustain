//! Fake MCP server binary for conformance tests.
//!
//! The fixture runs the real `rmcp` server machinery over stdio. Legacy
//! environment knobs remain available to existing MCP tests; `test/control/arm`
//! adds deterministic, re-armable task scripts for task-lifecycle tests.

// The `ServerHandler` trait declares RPITIT methods (`fn … -> impl Future`);
// clippy's suggestion to rewrite the impls as `async fn` would change the
// trait's object contract, so the manual form is intentional here.
#![allow(clippy::manual_async_fn)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientNotification, ClientRequest, Content,
    CustomRequest, CustomResult, ErrorCode, Implementation, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    ServerResult, Tool, ToolAnnotations,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, Service, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;

const CREATED_AT: &str = "2026-07-19T12:05:33Z";
const UPDATED_AT: &str = "2026-07-19T12:05:37Z";
const TTL_MS: u64 = 300_000;
const POLL_INTERVAL_MS: u64 = 1;

#[derive(Clone, Copy)]
enum TaskScript {
    Progress,
    InputRequired,
    /// 17.5b (AC2-a mutant enabler): two outstanding input-request keys;
    /// completes only when BOTH are answered. Synthetic (fake-authored, NOT a
    /// captured fixture — never added to manifest.json per C5).
    MultiInput,
    /// AC7: remains input-required long enough to observe the durable wait,
    /// then the server itself emits a terminal expiry signal. Synthetic,
    /// deterministic, and never added to the captured-fixture manifest.
    Expiry,
    Cancellation,
    Error,
    ChildExit,
    IsErrorCompleted,
    CancelReject,
}

impl TaskScript {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "progress" => Some(Self::Progress),
            "input-required" => Some(Self::InputRequired),
            "multi-input" => Some(Self::MultiInput),
            "expiry" => Some(Self::Expiry),
            "cancellation" => Some(Self::Cancellation),
            "error" => Some(Self::Error),
            "child-exit" => Some(Self::ChildExit),
            "isError-completed" => Some(Self::IsErrorCompleted),
            "cancel-reject" => Some(Self::CancelReject),
            _ => None,
        }
    }
}

struct Arm {
    remaining: u32,
    script: Option<TaskScript>,
}

struct TaskRecord {
    script: TaskScript,
    polls: u32,
    input_received: bool,
    /// 17.5b MultiInput: the input-request keys already answered. The task
    /// completes only when both `confirm` and `reason` are present.
    answered_keys: std::collections::HashSet<String>,
}

#[derive(Clone)]
struct FakeServer {
    fail_initialize: bool,
    hang_tools_list: bool,
    tool_error: bool,
    hang_call_tool: bool,
    emit_list_changed_after: Option<Duration>,
    list_changed_due: Arc<AtomicBool>,
    arms: Arc<Mutex<HashMap<String, Arm>>>,
    tasks: Arc<Mutex<HashMap<String, TaskRecord>>>,
    next_task: Arc<AtomicU32>,
}

impl FakeServer {
    fn from_env() -> Self {
        Self {
            fail_initialize: enabled("FAKE_MCP_FAIL_INITIALIZE"),
            hang_tools_list: enabled("FAKE_MCP_HANG_TOOLS_LIST"),
            tool_error: enabled("FAKE_MCP_TOOL_ERROR"),
            hang_call_tool: enabled("FAKE_MCP_HANG_CALL_TOOL"),
            emit_list_changed_after: duration_env("FAKE_MCP_EMIT_LIST_CHANGED_AFTER_MS"),
            list_changed_due: Arc::new(AtomicBool::new(false)),
            arms: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_task: Arc::new(AtomicU32::new(1)),
        }
    }
    fn start_list_changed_timer(&self) {
        if let Some(delay) = self.emit_list_changed_after {
            let due = Arc::clone(&self.list_changed_due);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                due.store(true, Ordering::SeqCst);
            });
        }
    }

    async fn emit_list_changed_if_due(&self, peer: &rmcp::Peer<RoleServer>) {
        if self.list_changed_due.swap(false, Ordering::SeqCst) {
            let _ = peer.notify_tool_list_changed().await;
        }
    }

    async fn arm(&self, params: Option<Value>) -> Result<Value, McpError> {
        let params =
            params.ok_or_else(|| McpError::invalid_params("missing arm parameters", None))?;
        let target = params
            .get("target")
            .and_then(Value::as_str)
            .filter(|target| !target.is_empty())
            .ok_or_else(|| {
                McpError::invalid_params("arm target must be a non-empty string", None)
            })?;
        let remaining = params
            .get("remaining")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| {
                McpError::invalid_params("arm remaining must be a positive u32", None)
            })?;
        let script = params
            .get("scenario")
            .or_else(|| params.get("case"))
            .and_then(Value::as_str)
            .or(Some(target))
            .and_then(TaskScript::parse);

        self.arms
            .lock()
            .await
            .insert(target.to_owned(), Arm { remaining, script });
        Ok(json!({ "resultType": "complete" }))
    }

    async fn consume_arm(&self, targets: &[&str]) -> Option<TaskScript> {
        let mut arms = self.arms.lock().await;
        for target in targets {
            let Some(arm) = arms.get_mut(*target) else {
                continue;
            };
            arm.remaining -= 1;
            let script = arm.script;
            if arm.remaining == 0 {
                arms.remove(*target);
            }
            return script;
        }
        None
    }

    async fn create_task(&self, script: TaskScript) -> Value {
        let task_id = format!(
            "fake-task-{}",
            self.next_task.fetch_add(1, Ordering::Relaxed)
        );
        self.tasks.lock().await.insert(
            task_id.clone(),
            TaskRecord {
                script,
                polls: 0,
                input_received: false,
                answered_keys: std::collections::HashSet::new(),
            },
        );
        task_create_reply(&task_id)
    }

    async fn task_get(&self, task_id: &str) -> Value {
        let mut tasks = self.tasks.lock().await;
        let Some(task) = tasks.get_mut(task_id) else {
            return missing_task_reply(task_id);
        };

        match task.script {
            TaskScript::Progress => {
                if task.polls == 0 {
                    task.polls += 1;
                    task_reply(task_id, "working", None)
                } else {
                    task_reply(
                        task_id,
                        "completed",
                        Some(json!({
                            "result": tool_result("progress completed", false),
                        })),
                    )
                }
            }
            TaskScript::InputRequired if !task.input_received => task_reply(
                task_id,
                "input_required",
                Some(json!({
                    "inputRequests": {
                        "confirm": {
                            "method": "elicitation/create",
                            "params": {
                                "message": "Confirm fake task?",
                                "requestedSchema": {
                                    "type": "object",
                                    "properties": { "confirm": { "type": "boolean" } },
                                    "required": ["confirm"]
                                },
                                "_meta": {
                                    "io.modelcontextprotocol/related-task": { "taskId": task_id }
                                }
                            }
                        }
                    }
                })),
            ),
            TaskScript::InputRequired => task_reply(
                task_id,
                "completed",
                Some(json!({ "result": tool_result("input accepted", false) })),
            ),
            TaskScript::MultiInput => {
                let both_answered =
                    task.answered_keys.contains("confirm") && task.answered_keys.contains("reason");
                if both_answered {
                    task_reply(
                        task_id,
                        "completed",
                        Some(json!({ "result": tool_result("both inputs accepted", false) })),
                    )
                } else {
                    // Re-emit BOTH outstanding keys until each is answered.
                    task_reply(
                        task_id,
                        "input_required",
                        Some(json!({
                            "inputRequests": {
                                "confirm": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "message": "Confirm?",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": { "confirm": { "type": "boolean" } },
                                            "required": ["confirm"]
                                        }
                                    }
                                },
                                "reason": {
                                    "method": "elicitation/create",
                                    "params": {
                                        "message": "Reason?",
                                        "requestedSchema": {
                                            "type": "object",
                                            "properties": { "reason": { "type": "string" } },
                                            "required": ["reason"]
                                        }
                                    }
                                }
                            }
                        })),
                    )
                }
            }
            TaskScript::Expiry if task.polls < 20 => {
                task.polls += 1;
                task_reply(
                    task_id,
                    "input_required",
                    Some(json!({
                        "inputRequests": {
                            "confirm": {
                                "method": "elicitation/create",
                                "params": {
                                    "message": "Answer before the remote task expires",
                                    "requestedSchema": {
                                        "type": "object",
                                        "properties": { "confirm": { "type": "boolean" } },
                                        "required": ["confirm"]
                                    }
                                }
                            }
                        }
                    })),
                )
            }
            TaskScript::Expiry => task_reply(
                task_id,
                "failed",
                Some(json!({
                    "statusMessage": "remote task TTL expired before input arrived",
                    "error": {
                        "code": -32001,
                        "message": "remote task TTL expired before input arrived",
                        "data": { "reason": "expired" }
                    }
                })),
            ),
            // Deliberately remains working after the cancel acknowledgement.
            TaskScript::Cancellation => task_reply(task_id, "working", None),
            // Acks nothing: tasks/cancel is REJECTED (see `task_cancel`); the
            // node must NOT be forged `Cancelled` (D1 mutant).
            TaskScript::CancelReject => task_reply(task_id, "working", None),
            TaskScript::Error => task_reply(
                task_id,
                "failed",
                Some(json!({
                    "statusMessage": "simulated protocol error",
                    "error": { "code": -32603, "message": "simulated protocol error" }
                })),
            ),
            TaskScript::ChildExit => task_reply(task_id, "working", None),
            TaskScript::IsErrorCompleted => task_reply(
                task_id,
                "completed",
                Some(json!({ "result": tool_result("simulated tool error", true) })),
            ),
        }
    }

    async fn task_cancel(&self, task_id: &str) -> Value {
        let tasks = self.tasks.lock().await;
        match tasks.get(task_id) {
            // Rejects cancellation with a NON-`complete` ack (not a handler
            // error, which rmcp surfaces as a transport closure). The driver
            // must refuse to forge `Cancelled` (R-15 / P9 / D1).
            Some(record) if matches!(record.script, TaskScript::CancelReject) => {
                json!({ "resultType": "rejected" })
            }
            Some(_) => json!({ "resultType": "complete" }),
            None => missing_task_reply(task_id),
        }
    }

    async fn task_update(&self, params: Option<Value>) -> Value {
        let Some(task_id) = params
            .as_ref()
            .and_then(|params| params.get("taskId"))
            .and_then(Value::as_str)
        else {
            return json!({ "resultType": "complete" });
        };
        // The answered keys arrive in `inputResponses` (R-6 correlation).
        let answered_keys: Vec<String> = params
            .as_ref()
            .and_then(|p| p.get("inputResponses"))
            .and_then(|v| v.as_object())
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(task) = self.tasks.lock().await.get_mut(task_id) {
            match task.script {
                TaskScript::InputRequired if !answered_keys.is_empty() => {
                    task.input_received = true;
                }
                // 17.5b MultiInput: completes only when BOTH keys are answered.
                TaskScript::MultiInput => {
                    for key in answered_keys {
                        task.answered_keys.insert(key);
                    }
                }
                _ => {}
            }
        }
        json!({ "resultType": "complete" })
    }

    fn tools() -> Vec<Tool> {
        vec![
            tool(
                "echo",
                "Echoes back the input text",
                json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
                false,
            ),
            tool(
                "add",
                "Adds two numbers",
                json!({
                    "type": "object",
                    "properties": {
                        "a": { "type": "number" },
                        "b": { "type": "number" }
                    }
                }),
                true,
            ),
        ]
    }
}

impl ServerHandler for FakeServer {
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, McpError>> + Send + '_ {
        async move {
            if self.fail_initialize {
                return Err(McpError::new(
                    ErrorCode::INTERNAL_ERROR,
                    "initialize rejected by FAKE_MCP_FAIL_INITIALIZE",
                    None,
                ));
            }
            if context.peer.peer_info().is_none() {
                context.peer.set_peer_info(request);
            }
            Ok(ServerHandler::get_info(self))
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            if self.hang_tools_list {
                std::future::pending::<Result<ListToolsResult, McpError>>().await
            } else {
                Ok(ListToolsResult::with_all_items(Self::tools()))
            }
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            if self.hang_call_tool {
                return std::future::pending::<Result<CallToolResult, McpError>>().await;
            }
            let name = request.name.as_ref();
            if self.tool_error {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "error from {name}"
                ))]));
            }
            let arguments = request.arguments.unwrap_or_default();
            let text = match name {
                "echo" => format!(
                    "echo: {}",
                    arguments.get("text").and_then(Value::as_str).unwrap_or("")
                ),
                "add" => format!(
                    "{}",
                    arguments.get("a").and_then(Value::as_f64).unwrap_or(0.0)
                        + arguments.get("b").and_then(Value::as_f64).unwrap_or(0.0)
                ),
                other => format!("unknown tool: {other}"),
            };
            Ok(CallToolResult::success(vec![Content::text(text)]))
        }
    }

    fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CustomResult, McpError>> + Send + '_ {
        async move {
            let value = match request.method.as_str() {
                "test/control/arm" => self.arm(request.params).await?,
                "tasks/get" => {
                    let task_id = request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("taskId"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.task_get(task_id).await
                }
                "tasks/cancel" => {
                    let task_id = request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("taskId"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.task_cancel(task_id).await
                }
                "tasks/update" => self.task_update(request.params).await,
                method => {
                    return Err(McpError::new(
                        ErrorCode::METHOD_NOT_FOUND,
                        method.to_owned(),
                        None,
                    ));
                }
            };
            Ok(CustomResult::new(value))
        }
    }

    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_protocol_version(rmcp::model::ProtocolVersion::V_2024_11_05)
        .with_server_info(Implementation::new("fake-mcp-server", "0.1.0"))
    }
}

/// `rmcp` correctly dispatches standard MCP methods into `ServerHandler`, but
/// its 2025 task variants cannot serialize the 2026 Tasks wire shape. This
/// service keeps normal MCP traffic on the real handler and only preserves the
/// task extension's observed bytes at that compatibility boundary.
struct FakeService(FakeServer);

impl Service<RoleServer> for FakeService {
    fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ServerResult, McpError>> + Send + '_ {
        async move {
            self.0.emit_list_changed_if_due(&context.peer).await;
            match request {
                ClientRequest::CallToolRequest(request) => {
                    let tool_name = request.params.name.to_string();
                    if let Some(script) = self.0.consume_arm(&[&tool_name, "tools/call"]).await {
                        let reply = self.0.create_task(script).await;
                        if matches!(script, TaskScript::ChildExit) {
                            tokio::spawn(async {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                                std::process::exit(0);
                            });
                        }
                        Ok(ServerResult::CustomResult(CustomResult::new(reply)))
                    } else {
                        Service::<RoleServer>::handle_request(
                            &self.0,
                            ClientRequest::CallToolRequest(request),
                            context,
                        )
                        .await
                    }
                }
                ClientRequest::GetTaskInfoRequest(request) => Ok(ServerResult::CustomResult(
                    CustomResult::new(self.0.task_get(&request.params.task_id).await),
                )),
                ClientRequest::CancelTaskRequest(request) => Ok(ServerResult::CustomResult(
                    CustomResult::new(self.0.task_cancel(&request.params.task_id).await),
                )),
                request => Service::<RoleServer>::handle_request(&self.0, request, context).await,
            }
        }
    }

    fn handle_notification(
        &self,
        notification: ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            self.0.emit_list_changed_if_due(&context.peer).await;
            Service::<RoleServer>::handle_notification(&self.0, notification, context).await
        }
    }

    fn get_info(&self) -> ServerInfo {
        ServerHandler::get_info(&self.0)
    }
}

fn enabled(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

fn duration_env(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
}

fn tool(name: &'static str, description: &'static str, schema: Value, read_only: bool) -> Tool {
    Tool::new(
        name,
        description,
        Arc::new(schema.as_object().expect("tool schema object").clone()),
    )
    .with_annotations(ToolAnnotations::new().read_only(read_only))
}

fn task_create_reply(task_id: &str) -> Value {
    json!({
        "resultType": "task",
        "taskId": task_id,
        "status": "working",
        "createdAt": CREATED_AT,
        "lastUpdatedAt": CREATED_AT,
        "ttlMs": TTL_MS,
        "pollIntervalMs": POLL_INTERVAL_MS,
    })
}

fn task_reply(task_id: &str, status: &str, extra: Option<Value>) -> Value {
    let mut reply = serde_json::Map::from_iter([
        ("resultType".into(), json!("complete")),
        ("taskId".into(), json!(task_id)),
        ("status".into(), json!(status)),
        ("createdAt".into(), json!(CREATED_AT)),
        (
            "lastUpdatedAt".into(),
            json!(if status == "working" || status == "input_required" {
                CREATED_AT
            } else {
                UPDATED_AT
            }),
        ),
        ("ttlMs".into(), json!(TTL_MS)),
        ("pollIntervalMs".into(), json!(POLL_INTERVAL_MS)),
        ("requestState".into(), json!(task_id)),
    ]);
    if let Some(Value::Object(extra)) = extra {
        reply.extend(extra);
    }
    Value::Object(reply)
}

fn missing_task_reply(task_id: &str) -> Value {
    task_reply(
        task_id,
        "failed",
        Some(json!({
            "statusMessage": "unknown fake task",
            "error": { "code": -32602, "message": "unknown fake task" }
        })),
    )
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "resultType": "complete",
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

#[tokio::main]
async fn main() {
    if let Some(delay) = duration_env("FAKE_MCP_DROP_AFTER_MS") {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            std::process::exit(0);
        });
    }

    let fake = FakeServer::from_env();
    fake.start_list_changed_timer();
    let service = FakeService(fake);
    match service
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
    {
        Ok(running) => {
            let _ = running.waiting().await;
        }
        Err(error) => eprintln!("fake-mcp-server failed: {error}"),
    }
}
